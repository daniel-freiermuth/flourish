<script lang="ts">
  import { store, post } from "../lib/api.svelte";
  import { ts, clock, countdown, dur, ktok } from "../lib/format";
  import type { AdminTask, JobPhase, NextCandidate } from "../lib/types";

  let btnText = $state("Run Cycle");
  let btnDisabled = $state(false);
  let schedulerBusy = $state(false);
  let overdriveBusy = $state(false);

  async function toggleScheduler() {
    if (!store.summary) return;
    schedulerBusy = true;
    try {
      const r = await post("/api/scheduler", { paused: !store.summary.scheduler_paused });
      if (!r.ok) {
        console.error(`scheduler control request refused: HTTP ${r.status}`);
        return;
      }
      await store.refresh();
    } catch (err) {
      console.error("scheduler control request failed", err);
    } finally {
      schedulerBusy = false;
    }
  }

  async function toggleOverdrive() {
    if (!store.summary) return;
    overdriveBusy = true;
    try {
      const r = await post("/api/overdrive", { enabled: !store.summary.scheduler_overdrive });
      if (!r.ok) {
        console.error(`overdrive control request refused: HTTP ${r.status}`);
        return;
      }
      await store.refresh();
    } catch (err) {
      console.error("overdrive control request failed", err);
    } finally {
      overdriveBusy = false;
    }
  }


  async function runCycle() {
    btnDisabled = true;
    try {
      const r = await post<unknown>("/api/cycle", {});
      if (r.status === 409) {
        // "busy" and "paused" both still woke the admin lane: while a cycle
        // runs or jobs are paused, the button syncs open PRs.
        btnText = "syncing PRs\u2026";
      } else {
        btnText = r.status === 202 ? "started\u2026" : "error";
      }
      store.refresh();
    } catch (err) {
      console.error("run cycle request failed", err);
      btnText = "error";
    } finally {
      // Always re-arm the button, even if the request never reached the server.
      setTimeout(() => {
        btnText = "Run Cycle";
        btnDisabled = false;
      }, 2500);
    }
  }

  function candidateLabel(nc: NextCandidate): string {
    return nc.is_finding ? `#${nc.id} ${nc.label || ""}` : (nc.label || "");
  }

  // Format <time data-ms="..."> elements in the backend-rendered HTML
  // using the browser's local timezone (the server can't know it).
  $effect(() => {
    if (!store.summary) return;
    // Wait one tick for {@html} to render into the DOM.
    requestAnimationFrame(() => {
      for (const el of document.querySelectorAll<HTMLTimeElement>("time[data-ms]")) {
        const ms = Number(el.dataset.ms);
        if (!ms) continue;
        const d = new Date(ms);
        el.textContent = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
      }
    });
  });

  const activityKind = $derived(store.summary?.activity_status?.kind ?? null);
  // No cycle can start (one is running, or jobs are paused): the button
  // still wakes the admin lane, which syncs open PRs.
  const syncOnly = $derived(
    !!store.summary && (store.summary.cycle_running || store.summary.scheduler_paused),
  );

  const ADMIN_TASK_LABEL: Record<AdminTask, string> = {
    pr_sync: "syncing open PRs",
    reap: "removing deleted repos' clones",
    fetch_ahead: "fetching repos ahead of their jobs",
    dependency_scan: "scanning dependencies with Renovate",
    sweep: "sweeping finished workspaces",
  };
  const JOB_PHASE_LABEL: Record<JobPhase, string> = {
    waiting_for_clone: "waiting for the repo's clone",
    cloning: "cloning the repo",
    fetching: "fetching the repo",
    preparing_tree: "preparing the job's worktree",
    reviewing: "reviewing the fix before it ships",
    publishing: "publishing to the PR",
    ingesting: "filing findings",
  };
</script>

<div class="page-enter">
  <h2 class="page-title">Status</h2>

  {#if store.summary}
    <div class="dashboard-grid">
      <!-- Backend windows card -->
      <div class="dash-card windows-card">
        {@html store.summary.backend_status_html}
      </div>

      <!-- Cycle control card -->
      <div class="dash-card cycle-card">
        <div class="mode-buttons">
          <button
            class="scheduler-btn"
            class:paused={store.summary.scheduler_paused}
            disabled={schedulerBusy}
            onclick={toggleScheduler}
            aria-label={store.summary.scheduler_paused ? "Resume jobs" : "Pause jobs"}
            title="Pauses new jobs. PR sync, repo reaping, fetch-ahead, Renovate scans and workspace cleanup keep running."
          >
            <span class="scheduler-icon">{store.summary.scheduler_paused ? "▶" : "⏸"}</span>
            <span>{store.summary.scheduler_paused ? "Resume jobs" : "Pause jobs"}</span>
          </button>
          <button
            class="overdrive-btn"
            class:active={store.summary.scheduler_overdrive}
            disabled={overdriveBusy}
            onclick={toggleOverdrive}
            aria-pressed={store.summary.scheduler_overdrive}
            title="Use prioritized budget for every job. Provider hard stops still apply."
          >
            {store.summary.scheduler_overdrive ? "Overdrive active" : "Enable overdrive"}
          </button>
        </div>
        <button
          class="cycle-btn"
          class:ready={activityKind === "ready" && !btnDisabled && !syncOnly}
          disabled={btnDisabled}
          onclick={runCycle}
          title={syncOnly ? "No cycle can start now; this syncs open PRs." : undefined}
        >
          {btnDisabled ? btnText : syncOnly ? "Sync PRs now" : btnText}
        </button>
        {#if store.summary.last_cycle}
          <div class="last-cycle">
            {ts(store.summary.last_cycle.at)} · {store.summary.last_cycle.kind}: {store.summary.last_cycle.message}
          </div>
        {/if}
      </div>
    </div>

    <!-- Activity status -->
    <div class="activity-card">
      {#if store.summary.activity_status.kind === "running"}
        {@const cj = store.summary.activity_status.job}
        {@const label = cj.finding_id != null
          ? `#${cj.finding_id} ${cj.finding_summary || cj.finding_fingerprint || ""}`
          : cj.repo_name}
        <div class="row">
          <span class="status-icon running">▶</span>
          <b class="running">running</b>
          <span>{cj.kind}: {label}</span>
          <span class="dim">({dur(cj.started_at, cj.finished_at)}, {ktok(cj.tokens_new)} tok, job #{cj.id})</span>
          {#if store.summary.job_activity}
            {@const ja = store.summary.job_activity}
            <span class="dim">&middot; {JOB_PHASE_LABEL[ja.phase]} ({dur(ja.started_at, null)})</span>
          {/if}
        </div>
      {:else if store.summary.activity_status.kind === "working"}
        <div class="row">
          <span class="status-icon running">▶</span>
          {#if store.summary.job_activity}
            {@const ja = store.summary.job_activity}
            <b class="running">running</b> {JOB_PHASE_LABEL[ja.phase]}&hellip;
            <span class="dim">({dur(ja.started_at, null)})</span>
          {:else}
            <b class="running">running</b> picking, preparing or finishing a job&hellip;
          {/if}
        </div>
      {:else if store.summary.activity_status.kind === "error"}
        <div class="row">
          <span class="status-icon error-icon">⚠</span>
          <b class="error">error</b> {store.summary.activity_status.detail}
        </div>
      {:else if store.summary.activity_status.kind === "offline"}
        {@const ss = store.summary.scheduler_state}
        <div class="row">
          <span class="status-icon paused-icon">⏸</span>
          <b class="paused">offline</b> {store.summary.activity_status.detail}
        </div>
        {#if ss?.next_wake_at}
          <div class="row dim sub-row">
            next check ~{countdown(ss.next_wake_at)} ({ts(ss.next_wake_at)})
          </div>
        {/if}
      {:else if store.summary.activity_status.kind === "paused"}
        {@const nc = store.summary.activity_status.candidate}
        {@const ss = store.summary.scheduler_state}
        <div class="row">
          <span class="status-icon paused-icon">⏸</span>
          <b class="paused">paused</b>
          <span>next up: {nc.kind} {candidateLabel(nc)}</span>
          <span>&middot; budget: <span class="b-denied">denied</span> ({nc.budget_reason})</span>
        </div>
        {#if nc.budget_retry_at}
          <div class="row dim sub-row">
            budget available ~{countdown(nc.budget_retry_at)} ({clock(nc.budget_retry_at)})
          </div>
        {/if}
        {#if ss?.next_wake_at}
          {@const heartbeatOnly = nc.budget_retry_at != null && ss.next_wake_at < nc.budget_retry_at}
          <div class="row dim sub-row">
            {heartbeatOnly ? "next sync check" : "next check"} ~{countdown(ss.next_wake_at)} ({ts(ss.next_wake_at)}){heartbeatOnly ? " \u2014 budget still closed" : ""}
          </div>
        {/if}
      {:else if store.summary.activity_status.kind === "ready"}
        {@const nc = store.summary.activity_status.candidate}
        {@const ss = store.summary.scheduler_state}
        <div class="row">
          <span class="status-icon ready-icon">🚀</span>
          <b class="ready-text">ready</b>
          <span>next up: {nc.kind} {candidateLabel(nc)}</span>
          <span>&middot; budget: <span class={nc.budget_state === "denied" ? "b-denied" : nc.budget_state === "allowed" ? "b-allowed" : "b-exempt"}>{nc.budget_state}</span></span>
        </div>
        {#if ss?.next_wake_at}
          <div class="row dim sub-row">
            starting ~{countdown(ss.next_wake_at)} ({ts(ss.next_wake_at)})
          </div>
        {/if}
      {:else if store.summary.activity_status.kind === "idle"}
        {@const ss = store.summary.scheduler_state}
        <div class="row">
          <span class="status-icon idle-icon">✓</span>
          <b class="idle">idle</b> nothing to do
        </div>
        {#if ss?.next_wake_at}
          <div class="row dim sub-row">
            next check ~{countdown(ss.next_wake_at)} ({ts(ss.next_wake_at)})
          </div>
        {/if}
      {:else if store.summary.activity_status.kind === "warming_up"}
        <div class="row dim">warming up &mdash; no cycle has run yet</div>
      {/if}

      <!-- Admin lane: its duties run beside the job lane above and each other -->
      {#each store.summary.admin_activity as aa (aa.task)}
        <div class="row dim">
          <span class="status-icon running">▶</span>
          admin: {ADMIN_TASK_LABEL[aa.task]} ({dur(aa.started_at, null)})
        </div>
      {/each}

      <!-- Scheduler detail (last log) -->
      {#if store.summary.scheduler_state}
        <div class="row last-log">{store.summary.scheduler_state.detail}</div>
      {/if}
    </div>
  {:else}
    <p class="loading">Loading&hellip;</p>
  {/if}
</div>

<style>
  /* ── Page ────────────────────────────────────────────────────── */
  .page-title {
    font-size: 1.25rem;
    font-weight: 700;
    margin-bottom: 1rem;
  }

  /* ── Dashboard grid ─────────────────────────────────────────── */
  .dashboard-grid {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 0.75rem;
    margin-bottom: 0.75rem;
  }

  .dash-card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 0.875rem;
  }

  .windows-card {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .cycle-card {
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    justify-content: center;
    min-width: 368px;
  }

  .mode-buttons {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 0.5rem;
  }

  .scheduler-btn,
  .overdrive-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 0.5rem;
    min-width: 180px;
    padding: 0.875rem 1rem;
    border-radius: var(--radius-md);
    font-size: 1rem;
    font-weight: 700;
  }
  .scheduler-btn {
    background: var(--bad);
    color: var(--bg);
  }
  .scheduler-btn:hover:not(:disabled),
  .overdrive-btn:hover:not(:disabled) { filter: brightness(1.12); }
  .scheduler-btn:disabled,
  .overdrive-btn:disabled { opacity: 0.55; cursor: default; }
  .scheduler-btn.paused { background: var(--ok); }
  .scheduler-icon { font-size: 1.25rem; line-height: 1; }

  .overdrive-btn {
    background: transparent;
    border: 1px solid var(--sev-medium);
    color: var(--sev-medium);
  }
  .overdrive-btn.active {
    background: var(--sev-medium);
    color: var(--bg);
  }

  .cycle-btn {
    margin-top: 0.625rem;
    padding: 0.375rem 0.75rem;
    border-radius: var(--radius-md);
    background: transparent;
    border: 1px solid var(--border);
    color: var(--text-dim);
    font-weight: 600;
    font-size: 0.75rem;
  }
  .cycle-btn:hover:not(:disabled) { color: var(--text); border-color: var(--accent); }
  .cycle-btn:disabled { opacity: 0.45; cursor: default; }
  .cycle-btn.ready { animation: readyPulse 2s ease-in-out infinite; }

  .last-cycle {
    font-size: 0.75rem;
    color: var(--text-dim);
    margin-top: 0.5rem;
    max-width: 260px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    text-align: right;
  }

  /* ── Loading ────────────────────────────────────────────────── */
  .loading {
    color: var(--text-dim);
    animation: pulse 2s ease-in-out infinite;
  }

  /* ── Backend-rendered scv-* classes (verbatim from Rust) ─────── */
  :global(.scv-win) { min-width: 190px; }
  :global(.scv-lab) { display: flex; justify-content: space-between; font-size: 0.875rem; margin-bottom: 2px; }
  :global(.scv-lab b) { font-weight: 600; }
  :global(.scv-bar) { position: relative; height: 14px; background: #2a2a3a; border-radius: var(--radius-sm); overflow: hidden; }
  :global(.scv-fill) { display: inline-block; height: 100%; }
  :global(.scv-fill.scv-ok) { background: #4e8; }
  :global(.scv-fill.scv-bad) { background: #e54; }
  :global(.scv-fill.scv-stale) { background: #888; }
  :global(.scv-soft) { display: inline-block; height: 100%; background: repeating-linear-gradient(45deg, transparent, transparent 3px, rgba(255,255,255,0.15) 3px, rgba(255,255,255,0.15) 6px); }
  :global(.scv-ramp) { position: absolute; top: 0; bottom: 0; width: 2px; background: #e54; }
  :global(.scv-sub) { font-size: 0.75rem; color: var(--text-dim); margin-top: 2px; }
  :global(.scv-note) { font-size: 0.875rem; color: var(--text-dim); }

  /* ── Activity card ──────────────────────────────────────────── */
  .activity-card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 0.75rem 0.875rem;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }
  .activity-card .row {
    font-size: 0.9375rem;
    display: flex;
    align-items: baseline;
    gap: 0.375rem;
    flex-wrap: wrap;
  }
  .activity-card .dim { color: var(--text-dim); }
  .activity-card .sub-row { font-size: 0.8125rem; padding-left: 1.5rem; }

  .status-icon {
    font-size: 0.8125rem;
    flex-shrink: 0;
  }
  .running       { color: var(--ok); font-weight: 600; }
  .ready-text    { color: var(--accent); font-weight: 600; }
  .paused        { color: var(--sev-medium); font-weight: 600; }
  .error         { color: var(--bad); font-weight: 600; }
  .idle          { color: var(--text-dim); font-weight: 600; }

  .last-log { color: rgba(255, 255, 255, 0.3); font-size: 0.75rem; margin-top: 0.25rem; }
  :global(.b-denied) { color: var(--bad); }
  :global(.b-allowed) { color: var(--ok); }
  :global(.b-exempt) { color: var(--accent); }

  /* ── Responsive ──────────────────────────────────────────────── */
  @media (max-width: 600px) {
    .dashboard-grid {
      grid-template-columns: 1fr;
    }
    .cycle-card {
      align-items: stretch;
      min-width: 0;
    }
    .mode-buttons {
      grid-template-columns: 1fr;
    }
    .last-cycle {
      text-align: left;
    }
  }
</style>
