<script lang="ts">
  import { store } from "../lib/api.svelte";
  import { ktok, pct } from "../lib/format";
  import { navigation } from "../lib/navigation.svelte";
  import { STATS_HORIZONS } from "../lib/statsHorizon";
  import type { StatsHorizon } from "../lib/types";

  let stats = $derived(store.stats);
  let totals = $derived(stats?.totals ?? null);
  let byKind = $derived(stats?.by_kind ?? []);
  let byFinding = $derived(stats?.by_finding ?? []);
  const horizons = Object.entries(STATS_HORIZONS) as [StatsHorizon, (typeof STATS_HORIZONS)[StatsHorizon]][];
</script>

<div class="page-enter">
  <div class="page">
    <div class="page-head">
      <h2 class="page-title">Stats</h2>
      <div class="horizon" role="group" aria-label="Time horizon">
        {#each horizons as [horizon, { label, window }] (horizon)}
          <button
            class="horizon-btn"
            class:active={store.statsHorizon === horizon}
            aria-pressed={store.statsHorizon === horizon}
            title={window}
            onclick={() => store.setStatsHorizon(horizon)}
          >
            {label}
          </button>
        {/each}
      </div>
    </div>
    <p class="horizon-caption">{STATS_HORIZONS[store.statsHorizon].window}</p>

    {#if !stats}
      <p class="loading">Loading stats…</p>
    {:else}
      <!-- Totals summary cards -->
      {#if totals}
        <div class="totals-grid">
          <div class="stat-card">
            <span class="stat-icon">⚙</span>
            <span class="stat-value">{totals.jobs}</span>
            <span class="stat-label">Jobs</span>
          </div>
          <div class="stat-card">
            <span class="stat-icon">◎</span>
            <span class="stat-value">{ktok(totals.total_tokens)}</span>
            <span class="stat-label">Tokens</span>
          </div>
          <div class="stat-card">
            <span class="stat-icon">↻</span>
            <span class="stat-value">{totals.total_calls ?? '–'}</span>
            <span class="stat-label">Calls</span>
          </div>
          <div class="stat-card">
            <span class="stat-icon">✓</span>
            <span class="stat-value ok">{totals.done ?? '–'}</span>
            <span class="stat-label">Done</span>
          </div>
          <div class="stat-card">
            <span class="stat-icon">✕</span>
            <span class="stat-value stale">{totals.denied ?? '–'}</span>
            <span class="stat-label">Denied</span>
          </div>
          <div class="stat-card">
            <span class="stat-icon">Δ</span>
            <span class="stat-value">{pct(totals.total_usage_delta)}</span>
            <span class="stat-label">Usage Δ</span>
          </div>
        </div>
      {/if}

      <!-- By Kind table -->
      {#if byKind.length > 0}
        <div class="card">
          <h3 class="section-title">By Kind</h3>
          <div class="table-wrap">
            <table class="data-table">
              <thead>
                <tr>
                  <th>Kind</th>
                  <th class="right">Jobs</th>
                  <th class="right">Done</th>
                  <th class="right">Failed</th>
                  <th class="right">Killed</th>
                  <th class="right">Denied</th>
                  <th class="right">Tokens</th>
                  <th class="right">Avg Tok</th>
                  <th class="right">Usage Δ</th>
                  <th>Models</th>
                </tr>
              </thead>
              <tbody>
                {#each byKind as row (row.kind)}
                  <tr>
                    <td class="mono accent">{row.kind}</td>
                    <td class="right">{row.jobs}</td>
                    <td class="right ok">{row.done ?? '–'}</td>
                    <td class="right bad">{row.failed ?? '–'}</td>
                    <td class="right warn">{row.killed ?? '–'}</td>
                    <td class="right stale">{row.denied ?? '–'}</td>
                    <td class="right mono">{ktok(row.total_tokens)}</td>
                    <td class="right mono">{ktok(row.avg_tokens)}</td>
                    <td class="right mono">{pct(row.total_usage_delta)}</td>
                    <td class="dim">{row.models ?? '–'}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        </div>
      {/if}

      <!-- By Finding table -->
      {#if byFinding.length > 0}
        <div class="card">
          <h3 class="section-title">By Finding</h3>
          <div class="table-wrap">
            <table class="data-table">
              <thead>
                <tr>
                  <th>Finding</th>
                  <th>Fingerprint</th>
                  <th>Status</th>
                  <th>Severity</th>
                  <th class="right">Jobs</th>
                  <th class="right">Tokens</th>
                  <th class="right">Calls</th>
                  <th class="right">Usage Δ</th>
                </tr>
              </thead>
              <tbody>
                {#each byFinding as row (row.finding_id)}
                  <tr>
                    <td>
                      <a
                        href={navigation.findingHref(row.finding_id)}
                        class="finding-link"
                      >
                        #{row.finding_id}
                      </a>
                    </td>
                    <td class="fingerprint">{row.fingerprint}</td>
                    <td>
                      <span class="status-badge">{row.status}</span>
                    </td>
                    <td>
                      <span class="sev-label sev-{row.severity}">{row.severity}</span>
                    </td>
                    <td class="right">{row.jobs}</td>
                    <td class="right mono">{ktok(row.total_tokens)}</td>
                    <td class="right mono">{row.total_calls ?? '–'}</td>
                    <td class="right mono">{pct(row.total_usage_delta)}</td>
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        </div>
      {/if}
    {/if}
  </div>
</div>

<style>
  /* ── Page ────────────────────────────────────────────────────── */
  .page {
    display: flex;
    flex-direction: column;
    gap: 1.25rem;
  }

  .page-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
    flex-wrap: wrap;
  }

  .page-title {
    font-size: 1.25rem;
    font-weight: 700;
  }

  /* ── Horizon selector ───────────────────────────────────────── */
  .horizon {
    display: inline-flex;
    gap: 0.25rem;
    padding: 0.1875rem;
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
  }

  .horizon-btn {
    padding: 0.25rem 0.625rem;
    font-size: 0.75rem;
    font-weight: 600;
    border-radius: var(--radius-sm);
    color: var(--text-dim);
    transition: background-color var(--transition), color var(--transition);
  }
  .horizon-btn:hover:not(.active) {
    background: rgba(255, 255, 255, 0.06);
    color: var(--text);
  }
  .horizon-btn.active {
    background: var(--accent);
    color: var(--bg);
  }

  .horizon-caption {
    margin-top: -0.75rem;
    color: var(--text-dim);
    font-size: 0.75rem;
  }

  .loading {
    color: var(--text-dim);
    font-size: 0.875rem;
    animation: pulse 2s ease-in-out infinite;
  }

  /* ── Totals grid ─────────────────────────────────────────────── */
  .totals-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: 0.625rem;
  }

  .stat-card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 0.75rem;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.125rem;
    transition: border-color var(--transition);
  }
  .stat-card:hover {
    border-color: var(--border-hover);
  }

  .stat-icon {
    font-size: 0.875rem;
    color: var(--text-dim);
    opacity: 0.45;
  }

  .stat-value {
    font-size: 1.5rem;
    font-weight: 700;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    letter-spacing: -0.02em;
  }
  .stat-value.ok { color: var(--ok); }
  .stat-value.stale { color: var(--stale); }

  .stat-label {
    font-size: 0.625rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-dim);
    font-weight: 500;
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
    padding: 0.625rem 1rem 0.375rem;
  }

  /* ── Table ──────────────────────────────────────────────────── */
  .table-wrap {
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
    padding: 0.5rem 0.75rem;
    position: sticky;
    top: 0;
    background: var(--bg-card);
    z-index: 1;
  }

  .data-table th.right {
    text-align: right;
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
    padding: 0.4375rem 0.75rem;
  }
  .data-table td.right { text-align: right; }
  .data-table td.mono,
  .mono { font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace; }
  .data-table td.accent,
  .accent { color: var(--accent); }
  .ok { color: var(--ok); }
  .bad { color: var(--bad); }
  .warn { color: var(--sev-medium); }
  .stale { color: var(--stale); }
  .dim { color: var(--text-dim); font-size: 0.6875rem; }

  /* ── Finding link ───────────────────────────────────────────── */
  .finding-link {
    color: var(--accent);
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
  }
  .finding-link:hover { color: var(--accent-hover); }

  /* ── Fingerprint cell ───────────────────────────────────────── */
  .fingerprint {
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    font-size: 0.6875rem;
    color: var(--text-dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 10rem;
  }

  /* ── Status badge ───────────────────────────────────────────── */
  .status-badge {
    font-size: 0.625rem;
    padding: 0.0625rem 0.375rem;
    border-radius: 99px;
    background: rgba(255, 255, 255, 0.06);
    font-weight: 500;
  }

  /* ── Severity label ─────────────────────────────────────────── */
  .sev-label {
    font-size: 0.6875rem;
    font-weight: 600;
    text-transform: uppercase;
  }
  .sev-high { color: var(--sev-high); }
  .sev-medium { color: var(--sev-medium); }
  .sev-low { color: var(--sev-low); }
</style>
