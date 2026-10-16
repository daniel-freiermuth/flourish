<script lang="ts">
  // The filter controls. Which findings they leave, and in what order, is
  // lib/findingFilter.ts: pages derive their list from the URL themselves.
  import type { FindingOut } from "../lib/types";
  import { SEV_RANK } from "../lib/format";
  import { Filter, needsSelector, restoreFilter } from "../lib/filter.svelte";
  import { navigation } from "../lib/navigation.svelte";
  import { hidingParams } from "../lib/findingFilter";
  import { readConfidence, readSelection, readSort, writeSelection, type FilterKey } from "../lib/filterUrl";
  import MultiSelect from "./MultiSelect.svelte";

  let {
    findings,
    prefix,
    showStatus = false,
    repoNames = new Map<number, string>(),
  }: {
    findings: FindingOut[];
    prefix: string;
    showStatus?: boolean;
    repoNames?: Map<number, string>;
  } = $props();

  // -- Extract unique values from the findings list -------------------------
  const repos = $derived([...new Set(findings.map((f) => repoNames.get(f.repo_id) ?? "unknown"))].sort());
  const types = $derived([...new Set(findings.map((f) => f.type))].sort());
  const categories = $derived([...new Set(findings.filter((f) => f.category).map((f) => f.category!))].sort());
  const severities = $derived([...new Set(findings.map((f) => f.severity))].sort((a, b) => (SEV_RANK[b] ?? 0) - (SEV_RANK[a] ?? 0)));
  const statuses = $derived([...new Set(findings.map((f) => f.status))].sort());

  // Rebuild from the URL, never from the current options: filters survive
  // loading, polling, refreshes, and same-page browser history navigation.
  // A plain object, not state: it only remembers what each derived last returned.
  const shown: Partial<Record<FilterKey, Filter>> = {};
  function fromUrl(key: FilterKey): Filter {
    return (shown[key] = restoreFilter(shown[key], readSelection(navigation.route.params, key)));
  }
  const repoFilter = $derived(fromUrl("repo"));
  const typeFilter = $derived(fromUrl("type"));
  const categoryFilter = $derived(fromUrl("class"));
  const severityFilter = $derived(fromUrl("severity"));
  const statusFilter = $derived(fromUrl("status"));
  const minConfidence = $derived(readConfidence(navigation.route.params));
  const sortBy = $derived(readSort(navigation.route.params));

  // A focused finding stays focused while it stays visible. A filter the
  // user picks that hides it drops the focus instead: their filter wins
  // over the link's finding. Only links arriving stale reveal a finding
  // (AllFindingsPage), so the user's own filter is never undone.
  const focused = $derived.by(() => {
    const id = navigation.route.focusId;
    return id == null ? undefined : findings.find((f) => f.id === id);
  });

  function keepsFocus(params: URLSearchParams): boolean {
    return !focused || hidingParams(focused, params, { repoNames, showStatus }).length === 0;
  }

  function selectFilter(key: FilterKey, filter: Filter) {
    navigation.updateParams((params) => writeSelection(params, key, filter.selection), false, keepsFocus);
  }

  function selectConfidence(value: number, replace: boolean) {
    navigation.updateParams(
      (params) => {
        if (value === 0) params.delete("confidence");
        else params.set("confidence", String(value));
      },
      replace,
      keepsFocus,
    );
  }

  // A slider drag is one history step, not one for every input event.
  let sliding = false;
  function changeConfidence(value: number) {
    selectConfidence(value, sliding);
    sliding = true;
  }
</script>

<div class="filter-bar">
  {#if needsSelector(repos, repoFilter)}
    <MultiSelect label="Repo" options={repos} filter={repoFilter} onChange={() => selectFilter("repo", repoFilter)} />
  {/if}

  {#if needsSelector(types, typeFilter)}
    <MultiSelect label="Type" options={types} filter={typeFilter} onChange={() => selectFilter("type", typeFilter)} />
  {/if}

  {#if needsSelector(categories, categoryFilter)}
    <MultiSelect label="Class" options={categories} filter={categoryFilter} onChange={() => selectFilter("class", categoryFilter)} />
  {/if}

  {#if needsSelector(severities, severityFilter)}
    <MultiSelect label="Severity" options={severities} filter={severityFilter} onChange={() => selectFilter("severity", severityFilter)} />
  {/if}

  {#if showStatus && needsSelector(statuses, statusFilter)}
    <MultiSelect label="Status" options={statuses} filter={statusFilter} onChange={() => selectFilter("status", statusFilter)} />
  {/if}

  <div class="control">
    <label for="{prefix}-conf" class="control-label">Conf ≥</label>
    <input
      id="{prefix}-conf"
      type="range"
      min="0" max="100" step="5"
      value={minConfidence}
      oninput={(event) => changeConfidence(Number(event.currentTarget.value))}
      onchange={() => { sliding = false; }}
      class="slider"
    />
    <span class="slider-value">{minConfidence}%</span>
  </div>

  <div class="control">
    <label for="{prefix}-sort" class="control-label">Sort</label>
    <select
      id="{prefix}-sort"
      value={sortBy}
      onchange={(event) => navigation.updateParams((params) => {
        const value = event.currentTarget.value;
        if (value === "severity") params.delete("sort");
        else params.set("sort", value);
      })}
      class="sort-select"
    >
      <option value="severity">Severity × Confidence</option>
      <option value="newest">Newest</option>
      <option value="updated">Recently updated</option>
      <option value="oldest">Oldest</option>
    </select>
  </div>
</div>

<style>
  .filter-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
    margin-bottom: 1rem;
    font-size: 0.75rem;
  }


  /* ── Controls ────────────────────────────────────────────────── */
  .control {
    display: flex;
    align-items: center;
    gap: 0.375rem;
  }

  .control-label {
    color: var(--text-dim);
    font-size: 0.6875rem;
  }

  /* Custom slider track + thumb */
  .slider {
    -webkit-appearance: none;
    appearance: none;
    width: 4.5rem;
    height: 4px;
    border-radius: 2px;
    background: rgba(255, 255, 255, 0.1);
    outline: none;
  }
  /* `:focus-visible` rather than `:focus`: the ring exists for keyboard reach,
     so leave it to the UA to decide when a pointer drag doesn't warrant one. */
  .slider:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 3px;
  }
  .slider::-webkit-slider-thumb {
    -webkit-appearance: none;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--accent);
    cursor: pointer;
    border: 2px solid var(--bg-panel);
    box-shadow: 0 0 0 1px rgba(78, 168, 222, 0.3);
  }
  .slider::-moz-range-thumb {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--accent);
    cursor: pointer;
    border: 2px solid var(--bg-panel);
  }
  .slider::-moz-range-track {
    height: 4px;
    border-radius: 2px;
    background: rgba(255, 255, 255, 0.1);
  }

  .slider-value {
    color: var(--text-dim);
    width: 2rem;
    text-align: right;
    font-size: 0.6875rem;
    font-family: ui-monospace, "SF Mono", "Cascadia Code", monospace;
  }

  .sort-select {
    background: var(--bg-panel);
    border: 1px solid var(--border);
    border-radius: 99px;
    padding: 0.1875rem 0.5rem;
    color: var(--text);
    font-size: 0.6875rem;
  }
  .sort-select:focus {
    outline: none;
    border-color: var(--accent);
  }

  /* ── Responsive ──────────────────────────────────────────────── */
  @media (max-width: 600px) {
    .filter-bar {
      gap: 0.375rem;
    }
    .slider {
      width: 3rem;
    }
  }
</style>
