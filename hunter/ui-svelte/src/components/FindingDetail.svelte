<script lang="ts">
  import { store } from "../lib/api.svelte";
  import type { FindingDetail } from "../lib/types";
  import { ktok, dur, datetime } from "../lib/format";

  let { findingId }: { findingId: number } = $props();

  let detail = $state<FindingDetail | null>(null);
  let loading = $state(true);
  let error = $state(false);

  // Guards against a stale in-flight response clobbering a newer one when
  // findingId changes mid-fetch: only the latest generation may write state.
  let generation = 0;
  // Which finding the rendered `detail` belongs to, so a poll-triggered
  // refetch can keep the panel on screen instead of flashing the spinner.
  let loadedId = -1;

  async function load() {
    const gen = ++generation;
    loading = loadedId !== findingId;
    error = false;
    let d: FindingDetail | null = null;
    try {
      d = await store.fetchFindingDetail(findingId);
    } catch (err) {
      console.error(`load detail for finding ${findingId} failed`, err);
    }
    if (gen !== generation) return;
    if (d) {
      detail = d;
      loadedId = findingId;
    } else {
      error = true;
    }
    loading = false;
  }

  $effect(() => {
    // Re-fetch when findingId changes, and when a poll landed new data —
    // that is exactly when the store dropped this finding's cached detail.
    // A closed panel is unmounted, so it has no effect and never fetches.
    void findingId;
    void store.revision;
    load();
  });

  // Absent when talking to a daemon that predates the field.
  let reviewRounds = $derived(detail?.fix_review_rounds ?? []);
</script>

{#if loading}
  <div class="loading">Loading detail…</div>
{:else if error}
  <div class="error" role="status">Failed to load finding detail.</div>
{:else if detail}
  <!-- Job history -->
  {#if detail.jobs.length > 0}
    <div class="section">
      <h4 class="section-title">Job History</h4>
      <div class="table-wrap">
        <table class="detail-table">
          <thead>
            <tr>
              <th>Kind</th>
              <th>State</th>
              <th class="right">Tokens</th>
              <th class="right">Duration</th>
              <th>Model</th>
              <th>Started</th>
            </tr>
          </thead>
          <tbody>
            {#each detail.jobs as job (job.id)}
              <tr>
                <td>{job.kind}</td>
                <td>
                  <span
                    class="state"
                    class:ok={job.state === "done"}
                    class:bad={job.state === "failed" || job.state === "killed"}
                    class:dim={job.state !== "done" && job.state !== "failed" && job.state !== "killed"}
                  >
                    {job.state}
                  </span>
                </td>
                <td class="right mono">{ktok(job.tokens_new)}</td>
                <td class="right mono">{dur(job.started_at, job.finished_at)}</td>
                <td class="dim">{job.model ?? "–"}</td>
                <td class="dim">{datetime(job.started_at)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </div>
  {:else}
    <div class="loading">No jobs recorded.</div>
  {/if}

  <!-- PR state -->
  {#if detail.pr_state}
    {@const pr = detail.pr_state}
    <div class="section">
      <h4 class="section-title">PR State</h4>
      <div class="pr-grid">
        <span class="pr-label">State</span>
        <span class="pr-value" class:ok={pr.state === "MERGED"} class:bad={pr.state === "CLOSED"}>{pr.state ?? "–"}</span>

        <span class="pr-label">Mergeable</span>
        <span class="pr-value" class:ok={pr.mergeable === "MERGEABLE"} class:bad={pr.mergeable === "CONFLICTING"} class:dim={pr.mergeable !== "MERGEABLE" && pr.mergeable !== "CONFLICTING"}>{pr.mergeable ?? "–"}</span>

        <span class="pr-label">Checks</span>
        <span class="pr-value" class:ok={pr.checks === "SUCCESS"} class:bad={pr.checks === "FAILURE"} class:dim={pr.checks !== "SUCCESS" && pr.checks !== "FAILURE"}>{pr.checks ?? "–"}</span>

        <span class="pr-label">Branch</span>
        <span class="mono">{pr.head_ref ?? "–"}</span>

        {#if pr.pr_number}
          <span class="pr-label">PR #</span>
          <span>{pr.pr_number}</span>
        {/if}

        {#if pr.needs_attention}
          <span class="pr-label">Attention</span>
          <span class="warn">{pr.needs_attention}</span>
        {/if}

        <span class="pr-label">Synced</span>
        <span class="dim">{datetime(pr.synced_at)}</span>
      </div>
    </div>
  {/if}

  <!-- The review a fix had before its PR opened, newest round first -->
  {#if reviewRounds.length > 0}
    <div class="section">
      <h4 class="section-title">Fix Review</h4>
      <ul class="rounds">
        {#each reviewRounds as round (round.id)}
          <li>
            <span
              class="state"
              class:ok={round.verdict === "ok"}
              class:bad={round.verdict === "failed"}
              class:dim={round.verdict !== "ok" && round.verdict !== "failed"}
            >
              {round.verdict}
            </span>
            <span class="dim">job {round.job_id ?? "–"} · {datetime(round.finished_at)}</span>
            {#if round.failure}
              <span class="dim">— {round.failure}</span>
            {/if}
            {#if round.issues.length > 0}
              <ul class="issues">
                {#each round.issues as issue (issue.id)}
                  <li>
                    <span class="kind" class:warn={issue.blocking}>
                      {issue.blocking ? "blocking " : ""}{issue.kind}
                    </span>
                    {issue.summary}
                  </li>
                {/each}
              </ul>
            {:else if round.verdict === "ok"}
              <span class="dim">— no issues</span>
            {/if}
          </li>
        {/each}
      </ul>
    </div>
  {/if}
{/if}

<style>
  .loading {
    color: var(--text-dim);
    font-size: 0.75rem;
    padding: 0.5rem 0;
    animation: pulse 2s ease-in-out infinite;
  }

  .error {
    color: var(--bad);
    font-size: 0.75rem;
    padding: 0.5rem 0;
  }

  .section {
    margin-top: 0.5rem;
  }
  .section + .section {
    margin-top: 0.75rem;
  }

  .section-title {
    font-size: 0.6875rem;
    font-weight: 600;
    color: var(--text-dim);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    margin-bottom: 0.3125rem;
  }

  .table-wrap {
    overflow-x: auto;
  }

  .detail-table {
    width: 100%;
    font-size: 0.75rem;
    border-collapse: collapse;
  }

  .detail-table thead tr {
    color: var(--text-dim);
    border-bottom: 1px solid var(--border);
  }

  .detail-table th {
    text-align: left;
    padding: 0.25rem 0.5rem 0.25rem 0;
    font-weight: 600;
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .detail-table th.right {
    text-align: right;
  }

  .detail-table tbody tr {
    border-bottom: 1px solid rgba(255, 255, 255, 0.03);
  }
  .detail-table tbody tr:nth-child(even) {
    background: rgba(255, 255, 255, 0.02);
  }
  .detail-table tbody tr:hover {
    background: rgba(255, 255, 255, 0.05);
  }

  .detail-table td {
    padding: 0.25rem 0.5rem 0.25rem 0;
  }

  .detail-table td.right {
    text-align: right;
  }

  .mono {
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
  }

  .dim,
  td.dim {
    color: var(--text-dim);
  }

  .state.ok { color: var(--ok); }
  .state.bad { color: var(--bad); }
  .state.dim { color: var(--text-dim); }

  .pr-grid {
    display: grid;
    grid-template-columns: auto 1fr;
    column-gap: 0.75rem;
    row-gap: 0.1875rem;
    font-size: 0.75rem;
  }

  .pr-label {
    color: var(--text-dim);
    font-size: 0.6875rem;
  }

  .pr-value.ok { color: var(--ok); }
  .pr-value.bad { color: var(--bad); }
  .pr-value.dim { color: var(--text-dim); }

  .warn {
    color: var(--sev-medium);
  }

  .rounds,
  .issues {
    list-style: none;
    margin: 0;
    padding: 0;
    font-size: 0.75rem;
  }
  .rounds > li + li {
    margin-top: 0.25rem;
  }
  .issues {
    padding-left: 1rem;
  }
  .kind {
    color: var(--text-dim);
    font-size: 0.6875rem;
  }
  .kind.warn {
    color: var(--sev-medium);
  }
</style>
