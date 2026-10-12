<script lang="ts">
  // A repo's work held for the operator, and the work that is failing
  // without a hold yet (WORK-MODEL.md §17). One block per repo: the Repos
  // page shows it under each repo, Kanban's "Needs you" for every repo
  // with a hold.
  import { store, post } from "../lib/api.svelte";
  import { datetime, holdReasonLabel, releaseActionLabel, repoWorkLabel } from "../lib/format";
  import type { RepoBrief } from "../lib/types";

  let { repo, showFailing = true }: { repo: RepoBrief; showFailing?: boolean } = $props();

  // Absent from a daemon that predates the fields.
  const holds = $derived(repo.holds ?? []);
  const failing = $derived(showFailing ? (repo.failing ?? []) : []);

  let busy = $state(false);

  async function release(kind: string, action: string) {
    busy = true;
    try {
      const r = await post("/api/hold/release", { repo_id: repo.id, kind, action });
      if (!r.ok) {
        // post() resolves on 4xx/5xx — a refused release must not look applied.
        console.error(`release ${action} of ${repo.name}'s ${kind} rejected with HTTP ${r.status}`);
        return;
      }
      await store.refresh();
    } catch (err) {
      console.error(`release ${action} of ${repo.name}'s ${kind} failed`, err);
    } finally {
      busy = false;
    }
  }
</script>

{#each holds as hold (hold.kind)}
  <div class="hold">
    <div class="hold-head">
      <strong>Needs you:</strong> {repo.name} {repoWorkLabel(hold.kind)} — {holdReasonLabel(hold.reason)}
      <span class="hold-at">since {datetime(hold.held_at)}</span>
    </div>
    <div class="hold-detail">{hold.detail || "(no detail recorded)"}</div>
    <div class="btn-row">
      {#each hold.actions as action (action)}
        <button
          class="btn {action === 'continue' ? 'btn-queue' : 'btn-muted'}"
          disabled={busy}
          aria-label="{releaseActionLabel(action)} {repo.name}'s {repoWorkLabel(hold.kind)}"
          onclick={() => release(hold.kind, action)}
        >{action === "restart" && hold.kind === "sync" ? "Retry now" : releaseActionLabel(action)}</button>
      {/each}
    </div>
  </div>
{/each}

{#each failing as f (f.kind)}
  <div class="failing">
    {repoWorkLabel(f.kind)} failing: {f.last_failure}
    <span class="streak">{f.streak} in a row</span>
  </div>
{/each}

<style>
  .hold {
    margin-top: 0.5rem;
    padding: 0.5rem 0.625rem;
    border-left: 2px solid var(--sev-medium);
    background: var(--bg-panel);
    font-size: 0.75rem;
  }
  .hold-at {
    color: var(--text-dim);
    margin-left: 0.375rem;
  }
  .hold-detail {
    margin: 0.25rem 0 0.375rem;
    white-space: pre-wrap;
    max-height: 8rem;
    overflow: auto;
    color: var(--text-dim);
  }
  .failing {
    padding: 0.375rem 0.625rem;
    font-size: 0.75rem;
    color: var(--sev-medium);
  }
  .streak {
    color: var(--text-dim);
    margin-left: 0.375rem;
  }
  .btn-row {
    display: flex;
    gap: 0.3125rem;
    flex-wrap: wrap;
  }
  .btn {
    padding: 0.1875rem 0.4375rem;
    font-size: 0.6875rem;
    border-radius: var(--radius-sm);
    font-weight: 500;
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.04);
  }
  .btn:disabled { opacity: 0.4; }
  .btn-queue:hover:not(:disabled) { background: rgba(68, 238, 136, 0.15); color: var(--ok); }
  .btn-muted:hover:not(:disabled) { background: rgba(255, 255, 255, 0.1); color: var(--text); }
</style>
