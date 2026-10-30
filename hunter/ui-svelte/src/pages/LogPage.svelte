<script lang="ts">
  import { store } from "../lib/api.svelte";
  import type { JobListEntry } from "../lib/types";
  import { datetime, ktok, dur } from "../lib/format";
  import { navigation } from "../lib/navigation.svelte";

  let events = $derived(store.events);
  let jobs = $derived(store.jobs);

  // Full precision for the tooltip; the column itself stays narrow.
  function exact(ms: number | null): string {
    return ms ? new Date(ms).toLocaleString() : "";
  }

  // A hunt can turn up a dozen findings; listing them all would push the
  // rest of the row off the table. The remainder stays reachable as a
  // tooltip rather than being silently dropped.
  const MAX_PRODUCED_LINKS = 3;

  // Absent rather than empty when talking to a daemon that predates the
  // field, so this is the one place that decides what "no data" means.
  function produced(job: JobListEntry): number[] {
    return job.produced_finding_ids ?? [];
  }

  // The daemon's event kinds (hunter-rs: `log_event(`). Ingest logs one kind
  // per finding type — `hunt` for bugs, the type's own name otherwise.
  function kindClass(kind: string): string {
    switch (kind) {
      // Work that landed.
      case "ship": return "kind-ok";
      case "error": return "kind-bad";
      // A new finding of any type is untriaged work, and a budget override is
      // a deliberate deviation: both want the eye.
      case "hunt":
      case "dep_update":
      case "test_gap":
      case "refactor":
      case "modernization":
      case "standards":
      case "override": return "kind-warn";
      // Nothing ran, or queued work was withdrawn.
      case "deny":
      case "unqueue": return "kind-stale";
      // Routine daemon and operator activity.
      case "cycle":
      case "prsync":
      case "fix":
      case "review":
      case "engage":
      case "harvest":
      case "recheck":
      case "verdict":
      case "repo": return "kind-accent";
      default: return "kind-default";
    }
  }

  function stateClass(state: string): string {
    switch (state) {
      case "done": return "state-ok";
      case "failed": return "state-bad";
      case "killed": return "state-warn";
      case "denied": return "state-stale";
      case "running": return "state-accent";
      default: return "state-default";
    }
  }
</script>

<div class="page-enter">
  <div class="page">
    <h2 class="page-title">Log</h2>

    <!-- Recent Jobs -->
    <div class="card">
      <h3 class="section-title">Recent Jobs</h3>
      {#if jobs.length === 0}
        <p class="empty">No jobs.</p>
      {:else}
        <div class="scroll-wrap">
          <table class="data-table">
            <thead>
              <tr>
                <th class="th-ts">ID</th>
                <th class="th-started">Started</th>
                <th class="th-kind">Kind</th>
                <th>Repo</th>
                <th class="th-kind">Finding</th>
                <th class="th-kind">State</th>
                <th class="right">Tokens</th>
                <th class="right">Calls</th>
                <th class="right">Duration</th>
                <th>Model</th>
              </tr>
            </thead>
            <tbody>
              {#each jobs as job (job.id)}
                <tr>
                  <td class="cell-ts">#{job.id}</td>
                  <!-- Date and time: 50 jobs routinely span several days, so
                       "2:05 PM" alone would be ambiguous. The exact instant,
                       to the second, is in the tooltip. -->
                  <td class="cell-started" title={exact(job.started_at)}>
                    {datetime(job.started_at)}
                  </td>
                  <td class="cell-kind">{job.kind}</td>
                  <td class="cell-small">{job.repo_name}</td>
                  <td class="cell-links">
                    <!-- Two relationships in one column: the finding a job
                         was given to work on, then the findings it filed.
                         Hunts only file; engage/harvest jobs can do both
                         (FOLLOW-UPS.json), shown as "F#2010 → F#3474". -->
                    {#if job.finding_id != null}
                      <a href={navigation.findingHref(job.finding_id)} class="finding-link">
                        F#{job.finding_id}
                      </a>
                    {/if}
                    {#if produced(job).length > 0}
                      {#if job.finding_id != null}<span class="dim">→</span>{/if}
                      {#each produced(job).slice(0, MAX_PRODUCED_LINKS) as fid (fid)}
                        <a href={navigation.findingHref(fid)} class="finding-link">F#{fid}</a>
                      {/each}
                      {#if produced(job).length > MAX_PRODUCED_LINKS}
                        <span class="dim" title={produced(job).join(", ")}>
                          +{produced(job).length - MAX_PRODUCED_LINKS}
                        </span>
                      {/if}
                    {/if}
                    {#if job.finding_id == null && produced(job).length === 0}
                      <span class="dim">–</span>
                    {/if}
                  </td>
                  <td>
                    <span class="badge {stateClass(job.state)}">
                      {job.state}
                    </span>
                  </td>
                  <td class="right mono">{ktok(job.tokens_new)}</td>
                  <td class="right mono">{job.calls ?? '–'}</td>
                  <td class="right mono">{dur(job.started_at, job.finished_at)}</td>
                  <td class="dim">{job.model ?? '–'}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </div>

    <!-- Recent Events -->
    <div class="card">
      <h3 class="section-title">Recent Events</h3>
      {#if events.length === 0}
        <p class="empty">No events.</p>
      {:else}
        <div class="scroll-wrap">
          <table class="data-table">
            <thead>
              <tr>
                <th class="th-started">Time</th>
                <th class="th-kind">Kind</th>
                <!-- The operator account behind the event; blank for the
                     scheduler's own. -->
                <th class="th-by">By</th>
                <th>Message</th>
                <th class="th-links right">Links</th>
              </tr>
            </thead>
            <tbody>
              {#each events as ev (ev.id)}
                <tr>
                  <!-- Date and time, with the exact instant in the tooltip:
                       the event list spans days, so a bare time is ambiguous. -->
                  <td class="cell-started" title={exact(ev.at)}>{datetime(ev.at)}</td>
                  <td>
                    <span class="badge {kindClass(ev.kind)}">
                      {ev.kind}
                    </span>
                  </td>
                  <td class="dim">{ev.username ?? ""}</td>
                  <td class="cell-msg" title={ev.message}>{ev.message}</td>
                  <td class="cell-links">
                    {#if ev.finding_id != null}
                      <a href={navigation.findingHref(ev.finding_id)} class="finding-link">
                        F#{ev.finding_id}
                      </a>
                    {/if}
                    {#if ev.job_id != null}
                      <span class="dim">J#{ev.job_id}</span>
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </div>
  </div>
</div>

<style>
  /* ── Page ────────────────────────────────────────────────────── */
  .page {
    display: flex;
    flex-direction: column;
    gap: 1.25rem;
  }

  .page-title {
    font-size: 1.25rem;
    font-weight: 700;
  }

  /* ── Card ────────────────────────────────────────────────────── */
  .card {
    background: var(--bg-card);
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
    overflow: hidden;
  }

  .section-title {
    font-size: 0.75rem;
    font-weight: 600;
    color: var(--text-dim);
    text-transform: uppercase;
    letter-spacing: 0.04em;
    padding: 0.625rem 0.875rem 0.375rem;
  }

  .empty {
    color: var(--text-dim);
    font-size: 0.8125rem;
    padding: 0.5rem 0.875rem 0.75rem;
    animation: pulse 2s ease-in-out infinite;
  }

  /* ── Table ──────────────────────────────────────────────────── */
  .scroll-wrap {
    overflow-x: auto;
  }

  .data-table {
    width: 100%;
    font-size: 0.8125rem;
    border-collapse: collapse;
  }

  .data-table thead tr {
    border-bottom: 1px solid var(--border-hover);
    color: var(--text-dim);
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .data-table th {
    text-align: left;
    padding: 0.4375rem 0.75rem;
    position: sticky;
    top: 0;
    background: var(--bg-card);
    z-index: 1;
  }

  .data-table tbody tr {
    border-bottom: 1px solid rgba(255, 255, 255, 0.03);
    transition: background-color var(--transition);
  }
  .data-table tbody tr:nth-child(even) {
    background: rgba(255, 255, 255, 0.015);
  }
  .data-table tbody tr:hover {
    background: rgba(255, 255, 255, 0.04);
  }

  .data-table td {
    padding: 0.375rem 0.75rem;
  }

  .right { text-align: right; }

  /* ── Cell types ──────────────────────────────────────────────── */
  .cell-ts {
    color: var(--text-dim);
    font-size: 0.6875rem;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    white-space: nowrap;
    width: 7rem;
  }
  .th-ts { width: 7rem; }
  .th-started { width: 9rem; }
  .cell-started {
    color: var(--text-dim);
    font-size: 0.6875rem;
    white-space: nowrap;
  }

  .cell-kind {
    color: var(--accent);
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    font-size: 0.6875rem;
  }
  .th-kind { width: 6rem; }
  .th-by { width: 6rem; }

  .cell-msg {
    font-size: 0.75rem;
    max-width: 32rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .cell-links {
    text-align: right;
    white-space: nowrap;
    font-size: 0.6875rem;
  }
  .th-links { width: 6rem; }

  .cell-small {
    font-size: 0.6875rem;
  }

  .mono {
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    font-size: 0.6875rem;
  }

  .dim {
    color: var(--text-dim);
    font-size: 0.6875rem;
  }

  /* ── Badge ──────────────────────────────────────────────────── */
  .badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.375rem;
    border-radius: 99px;
    font-weight: 500;
    white-space: nowrap;
  }

  .finding-link {
    color: var(--accent);
    margin-right: 0.375rem;
    font-size: 0.6875rem;
  }
  .finding-link:hover { color: var(--accent-hover); }

  /* kind badge colors */
  .kind-ok      { background: rgba(68, 238, 136, 0.1); color: var(--ok); }
  .kind-bad     { background: rgba(238, 85, 68, 0.1); color: var(--bad); }
  .kind-warn    { background: rgba(255, 153, 0, 0.1); color: var(--sev-medium); }
  .kind-stale   { background: rgba(136, 136, 136, 0.1); color: var(--stale); }
  .kind-accent  { background: rgba(78, 168, 222, 0.1); color: var(--accent); }
  .kind-default { background: rgba(255, 255, 255, 0.04); color: var(--text-dim); }

  /* state badge colors */
  .state-ok      { background: rgba(68, 238, 136, 0.1); color: var(--ok); }
  .state-bad     { background: rgba(238, 85, 68, 0.1); color: var(--bad); }
  .state-warn    { background: rgba(255, 153, 0, 0.1); color: var(--sev-medium); }
  .state-stale   { background: rgba(136, 136, 136, 0.1); color: var(--stale); }
  .state-accent  { background: rgba(78, 168, 222, 0.1); color: var(--accent); }
  .state-default { background: rgba(255, 255, 255, 0.04); color: var(--text-dim); }
</style>
