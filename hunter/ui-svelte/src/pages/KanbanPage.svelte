<script lang="ts">
  import { SvelteSet } from "svelte/reactivity";
  import { store, post } from "../lib/api.svelte";
  import { navigation } from "../lib/navigation.svelte";
  import { filterFindings } from "../lib/findingFilter";
  import { Filter } from "../lib/filter.svelte";
  import { readSelection } from "../lib/filterUrl";
  import { loadFoldedColumns, saveFoldedColumns } from "../lib/kanbanFolds";
  import type { FindingOut } from "../lib/types";
  import FilterBar from "../components/FilterBar.svelte";
  import FindingCard from "../components/FindingCard.svelte";
  import RepoWork from "../components/RepoWork.svelte";

  const repoNames = $derived(new Map((store.summary?.repos ?? []).map((r) => [r.id, r.name])));

  // Work held for the operator (WORK-MODEL.md §9): absent from a daemon
  // that predates the field. Held findings wait in their own column, not
  // in the column of a status nothing will move them out of until a
  // release, whatever that status is.
  const isHeld = (f: FindingOut) => (f.holds ?? []).length > 0;

  const COLUMNS = [
    { key: "held", label: "Held", filter: isHeld },
    { key: "rechecking", label: "Rechecking", filter: (f: FindingOut) => f.status === "rechecking" },
    { key: "queued", label: "Queued", filter: (f: FindingOut) => f.status === "queued" },
    { key: "fixing", label: "Fixing", filter: (f: FindingOut) => f.status === "fixing" },
    { key: "pr_review", label: "PR Review", filter: (f: FindingOut) => f.status === "pr_open" && !!f.needs_attention },
    { key: "pr_open", label: "PR Open", filter: (f: FindingOut) => f.status === "pr_open" && !f.needs_attention },
    { key: "merged", label: "Merged", filter: (f: FindingOut) => f.status === "merged" },
    // Closed unmerged and not yet harvested: still in flight, because the
    // harvest has yet to say whether the finding was wrong or superseded.
    { key: "closed", label: "Closed", filter: (f: FindingOut) => f.status === "closed" },
  ];

  // Active pipeline and held work share the same filters.
  const pipelineFindings = $derived(
    store.findings.filter((f) => COLUMNS.some((c) => c.filter(f)))
  );

  const filtered = $derived(filterFindings(pipelineFindings, navigation.route.params, { repoNames, showStatus: false }));

  // Group filtered findings by column; a held finding shows only in Held.
  const columns = $derived(
    COLUMNS.map((col) => ({
      ...col,
      findings: filtered.filter((f) => (col.key === "held") === isHeld(f) && col.filter(f)),
      attentionCount: col.key === "pr_review"
        ? filtered.filter((f) => !isHeld(f) && f.status === "pr_open" && !!f.needs_attention).length
        : 0,
    }))
  );

  // A repo's held scans (WORK-MODEL.md §17) wait beside held findings,
  // under the same repo filter. Absent from a daemon that predates the
  // field.
  const reposWithHolds = $derived(
    (store.summary?.repos ?? []).filter((r) => (r.holds ?? []).length > 0)
  );
  const repoFilter = $derived(new Filter(readSelection(navigation.route.params, "repo")));
  const heldRepos = $derived(reposWithHolds.filter((r) => repoFilter.accepts(r.name)));
  const hasHeldRepos = $derived(reposWithHolds.length > 0);

  // Suppressed (rejected + wontfix) and notes — below the pipeline
  const suppressed = $derived(
    store.findings.filter((f) => f.status === "rejected" || f.status === "wontfix")
  );
  const notes = $derived(
    store.findings.filter((f) => f.status === "note")
  );
  // The finding was valid and its work landed another way. Not
  // suppressed, so kept apart from the section above.
  const superseded = $derived(
    store.findings.filter((f) => f.status === "superseded")
  );

  // Collapsible state for suppressed/superseded/notes — collapsed by default
  let suppressedOpen = $state(false);
  let supersededOpen = $state(false);
  let notesOpen = $state(false);

  // Folded columns shrink to a strip with name and counts, e.g. for the
  // ones that are empty almost all the time (Rechecking, Closed).
  const folded = new SvelteSet<string>(loadFoldedColumns());

  function toggleFold(key: string) {
    if (!folded.delete(key)) folded.add(key);
    saveFoldedColumns(folded);
  }

  // Findings with a verdict in flight. Both buttons write the same row
  // with an unconditional UPDATE, so letting them overlap means the last
  // response to land decides the status — and the card keeps rendering
  // the old one until the next poll.
  let pending = new SvelteSet<number>();

  async function verdict(id: number, status: string, reason?: string) {
    if (pending.has(id)) return;
    pending.add(id);
    try {
      const r = await post("/api/verdict", { id, status, ...(reason ? { reason } : {}) });
      if (!r.ok) {
        // post() resolves on 4xx/5xx — a rejected write must not look applied.
        console.error(`verdict ${status} for finding ${id} rejected with HTTP ${r.status}`);
        return;
      }
      await store.refresh();
    } catch (err) {
      console.error(`verdict ${status} for finding ${id} failed`, err);
    } finally {
      pending.delete(id);
    }
  }
</script>

<div class="page-enter">
  <h2 class="page-title">
    KANBAN / PIPELINE
    <span class="count-badge">{filtered.length}</span>
  </h2>

  <FilterBar
    findings={pipelineFindings}
    prefix="k"
    showStatus={false}
    repoNames={repoNames}
  />

  <div class="board" data-scroll-x="board">
    {#each columns as col (col.label)}
      {@const isFolded = folded.has(col.key)}
      <div class="column" class:folded={isFolded} class:held={col.key === "held"} class:empty={col.findings.length === 0}>
        <button
          class="col-header"
          aria-expanded={!isFolded}
          title="{isFolded ? 'Unfold' : 'Fold'} {col.label}"
          onclick={() => toggleFold(col.key)}
        >
          <span class="toggle-arrow">{isFolded ? "▸" : "▾"}</span>
          <span class="col-name">{col.label}</span>
          <span class="col-count">{col.findings.length}</span>
          {#if col.attentionCount > 0}
            <span class="attn-badge" title="{col.attentionCount} need attention">⚠ {col.attentionCount}</span>
          {/if}
        </button>
        {#if isFolded}
          <!-- Nothing below the header: the strip is the whole column. -->
        {:else if col.findings.length === 0}
          <p class="col-empty">—</p>
        {:else}
          <div class="col-cards" data-scroll-list={col.label}>
            {#each col.findings as finding (finding.id)}
              <FindingCard {finding} actions={col.key === "held"} />
            {/each}
          </div>
        {/if}
      </div>
    {/each}
  </div>

  {#if hasHeldRepos}
    <section class="held-section" aria-labelledby="held-heading">
      <h3 id="held-heading" class="held-heading">
        Held scans
        <span class="section-count">{heldRepos.length}</span>
      </h3>
      <p class="held-help">Repo work stopped for you: a missing prerequisite, a failure that kept repeating, or a chain that ran out of budget. Held findings wait in the Held column. Continue resumes the retained work with a fresh budget; restart starts it over.</p>
      {#if heldRepos.length === 0}
        <p class="held-help">No held scans match the current repo filter.</p>
      {:else}
        <div class="section-body" data-scroll-list="held-repos">
          {#each heldRepos as repo (repo.id)}
            <RepoWork {repo} showFailing={false} />
          {/each}
        </div>
      {/if}
    </section>
  {/if}

  <!-- Suppressed (rejected / wontfix) — collapsible -->
  {#if suppressed.length > 0}
    <div class="collapsible-section">
      <button class="section-toggle" aria-expanded={suppressedOpen} onclick={() => { suppressedOpen = !suppressedOpen; }}>
        <span class="toggle-arrow">{suppressedOpen ? "▾" : "▸"}</span>
        Suppressed
        <span class="section-count">{suppressed.length}</span>
      </button>
      {#if suppressedOpen}
        <div class="section-body">
          {#each suppressed as f (f.id)}
            <div class="item-card">
              <div class="item-header">
                <span class="item-badge">{f.status}</span>
                <span class="item-id">#{f.id}</span>
                <span class="item-fp">{f.fingerprint}</span>
              </div>
              <div class="item-summary">{f.summary}</div>
              <div class="item-detail">{f.verdict_reason ?? "(no reason)"}</div>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}

  <!-- Superseded — collapsible -->
  {#if superseded.length > 0}
    <div class="collapsible-section">
      <button class="section-toggle" aria-expanded={supersededOpen} onclick={() => { supersededOpen = !supersededOpen; }}>
        <span class="toggle-arrow">{supersededOpen ? "▾" : "▸"}</span>
        Superseded
        <span class="section-count">{superseded.length}</span>
      </button>
      {#if supersededOpen}
        <div class="section-body">
          {#each superseded as f (f.id)}
            <div class="item-card">
              <div class="item-header">
                <span class="item-badge superseded">superseded</span>
                <span class="item-id">#{f.id}</span>
                <span class="item-fp">{f.fingerprint}</span>
              </div>
              <div class="item-summary">{f.summary}</div>
              <div class="item-detail">{f.verdict_reason ?? "(no reason)"}</div>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}

  <!-- Notes — collapsible -->
  {#if notes.length > 0}
    <div class="collapsible-section">
      <button class="section-toggle" aria-expanded={notesOpen} onclick={() => { notesOpen = !notesOpen; }}>
        <span class="toggle-arrow">{notesOpen ? "▾" : "▸"}</span>
        Notes
        <span class="section-count">{notes.length}</span>
      </button>
      {#if notesOpen}
        <div class="section-body">
          {#each notes as f (f.id)}
            <div class="item-card">
              <div class="item-header">
                <span class="item-badge note">note</span>
                <span class="item-id">#{f.id}</span>
                <span class="item-fp">{f.fingerprint}</span>
              </div>
              <div class="item-summary">{f.summary}</div>
              <div class="item-detail">{f.file ?? ""}{f.line ? `:${f.line}` : ""}</div>
              <div class="item-actions">
                <button
                  class="action-btn queue-btn"
                  disabled={pending.has(f.id)}
                  onclick={() => verdict(f.id, "queued")}
                >Queue fix</button>
                <button
                  class="action-btn reject-btn"
                  disabled={pending.has(f.id)}
                  onclick={() => {
                    const reason = prompt("Rejection reason:");
                    if (reason) verdict(f.id, "rejected", reason);
                  }}
                >Reject</button>
              </div>
            </div>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .page-title {
    font-size: 1.25rem;
    font-weight: 700;
    margin-bottom: 1rem;
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .count-badge {
    font-size: 0.75rem;
    font-weight: 500;
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.05);
    padding: 0.0625rem 0.5rem;
    border-radius: 99px;
  }

  /* ── Board ──────────────────────────────────────────────────── */
  /* Flex rather than a grid: a folded column keeps only the width of its
     strip and the open ones share the rest. */
  .board {
    display: flex;
    gap: 0.625rem;
    min-height: 200px;
  }

  .column {
    background: var(--bg-panel);
    border-radius: var(--radius-md);
    padding: 0 0.375rem 0.5rem;
    border: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    overflow: hidden;
    min-width: 0;
    flex: 1 1 0;
  }
  .column.folded {
    flex: 0 0 auto;
    padding: 0;
  }
  /* Held work waits for the operator: amber, like the hold blocks. */
  .column.held {
    border-color: var(--sev-medium);
  }
  .column.held .col-name {
    color: var(--sev-medium);
  }

  .col-header {
    position: sticky;
    top: 0;
    z-index: 2;
    background: var(--bg-panel);
    padding: 0.5rem 0.25rem 0.375rem;
    display: flex;
    align-items: center;
    gap: 0.375rem;
    border: none;
    border-bottom: 1px solid var(--border);
    margin-bottom: 0.375rem;
    width: 100%;
    cursor: pointer;
    text-align: left;
  }
  .col-header:hover .col-name {
    color: var(--text);
  }
  .folded .col-header {
    flex-direction: column;
    flex: 1;
    padding: 0.5rem 0.375rem;
    border-bottom: none;
    margin-bottom: 0;
  }
  .folded .col-name {
    writing-mode: vertical-rl;
  }

  .col-name {
    text-transform: uppercase;
    font-size: 0.625rem;
    font-weight: 600;
    letter-spacing: 0.06em;
    color: var(--text-dim);
  }

  .col-count {
    font-size: 0.625rem;
    color: rgba(136, 136, 136, 0.6);
  }

  .attn-badge {
    padding: 0 0.25rem;
    border-radius: 99px;
    background: rgba(238, 85, 68, 0.12);
    color: var(--sev-high);
    font-size: 0.5625rem;
  }

  .col-empty {
    color: rgba(136, 136, 136, 0.35);
    font-size: 0.75rem;
    text-align: center;
    padding: 1rem 0;
  }

  .col-cards {
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
  }
  .col-cards > :global(.card) {
    margin-bottom: 0;
  }

  .held-section {
    margin-top: 1.25rem;
    padding: 0.75rem;
    border: 1px solid var(--sev-medium);
    border-radius: var(--radius-md);
    background: var(--bg-panel);
  }

  .held-heading {
    display: flex;
    align-items: center;
    gap: 0.375rem;
    margin: 0;
    color: var(--sev-medium);
    font-size: 0.875rem;
  }

  .held-help {
    margin: 0.375rem 0 0.625rem;
    color: var(--text-dim);
    font-size: 0.75rem;
  }

  /* ── Collapsible sections ───────────────────────────────────── */
  .collapsible-section {
    margin-top: 1.25rem;
  }

  .section-toggle {
    display: flex;
    align-items: center;
    gap: 0.375rem;
    font-size: 0.75rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--text-dim);
    padding: 0.375rem 0;
    cursor: pointer;
    background: none;
    border: none;
  }
  .section-toggle:hover {
    color: var(--text);
  }

  .toggle-arrow {
    font-size: 0.625rem;
    width: 0.75rem;
  }

  .section-count {
    font-size: 0.625rem;
    font-weight: 500;
    color: rgba(136, 136, 136, 0.6);
    background: rgba(255, 255, 255, 0.04);
    padding: 0 0.3125rem;
    border-radius: 99px;
  }

  .section-body {
    animation: fadeIn 150ms ease both;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    margin-top: 0.25rem;
  }

  /* ── Item cards ─────────────────────────────────────────────── */
  .item-card {
    background: var(--bg-card);
    border-radius: var(--radius-sm);
    padding: 0.5rem 0.75rem;
    border: 1px solid var(--border);
    font-size: 0.8125rem;
    transition: border-color var(--transition);
  }
  .item-card:hover {
    border-color: var(--border-hover);
  }

  .item-header {
    display: flex;
    align-items: center;
    gap: 0.375rem;
  }

  .item-badge {
    padding: 0 0.3125rem;
    border-radius: var(--radius-sm);
    background: rgba(255, 255, 255, 0.06);
    color: var(--text-dim);
    font-size: 0.625rem;
    font-weight: 500;
  }
  .item-badge.note {
    background: rgba(78, 168, 222, 0.12);
    color: var(--accent);
  }
  .item-badge.superseded {
    background: rgba(68, 238, 136, 0.12);
    color: var(--ok);
  }
  .item-id {
    color: var(--text-dim);
    font-size: 0.75rem;
  }
  .item-fp {
    color: var(--text-dim);
    font-size: 0.6875rem;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .item-summary {
    color: var(--text);
    margin-top: 0.125rem;
  }
  .item-detail {
    color: var(--text-dim);
    font-size: 0.6875rem;
    margin-top: 0.125rem;
  }
  .item-actions {
    display: flex;
    gap: 0.375rem;
    margin-top: 0.25rem;
  }
  .action-btn {
    font-size: 0.625rem;
    padding: 0.125rem 0.4375rem;
    border-radius: var(--radius-sm);
    font-weight: 500;
    color: var(--text-dim);
    background: rgba(255, 255, 255, 0.04);
  }
  .queue-btn:hover { background: rgba(255, 153, 0, 0.15); color: var(--sev-medium); }
  .reject-btn:hover { background: rgba(238, 85, 68, 0.15); color: var(--sev-high); }

  /* ── Responsive ──────────────────────────────────────────────── */
  @media (max-width: 1200px) {
    .board {
      flex-wrap: wrap;
    }
    .column:not(.folded) {
      flex-basis: calc((100% - 2 * 0.625rem) / 3);
    }
  }

  @media (max-width: 767px) {
    .board {
      display: flex;
      /* Undo the ≤1200px wrap: phones scroll the columns sideways. */
      flex-wrap: nowrap;
      overflow-x: auto;
      scroll-snap-type: x mandatory;
      gap: 0.625rem;
      -webkit-overflow-scrolling: touch;
      /* hide scrollbar but keep scroll */
      scrollbar-width: none;
    }
    .board::-webkit-scrollbar {
      display: none;
    }
    .column:not(.folded) {
      width: 85vw;
      min-width: 85vw;
      max-width: 85vw;
      scroll-snap-align: start;
      flex-shrink: 0;
    }
    .column.folded {
      scroll-snap-align: start;
    }
    /* Hide empty columns on mobile, folded or not: a folded column
       renders no .col-empty, so this keys on the count instead. */
    .column.empty {
      display: none;
    }
  }
</style>
