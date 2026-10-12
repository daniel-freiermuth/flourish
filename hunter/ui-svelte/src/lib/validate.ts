// Structural checks for the polled API responses.
//
// `api<T>()` casts whatever JSON came back to `T` without looking at it, so
// the type parameter is a claim about the server, not a fact about the
// bytes. The claim holds for the real daemon; it does not hold for the
// things that sit between the two in practice — a proxy error page served
// as 200 with a JSON content type, a captive portal, a truncated body, or
// a daemon mid-rollback answering an older schema.
//
// The previous guard only checked top-level categories: object vs array.
// `{}` is an object, so an empty body passed as a `Summary` and was written
// into the store, and `StatusPage` then read `summary.activity_status.kind`
// off `undefined`.
//
// These validators deliberately check only what the UI actually
// dereferences without a `?.`, which is why they are not a schema mirror:
// a field the components already treat as optional does not need to be
// present for the dashboard to render, and requiring it would turn a
// harmless server addition into a blank page. The rule for adding to this
// file is the same rule that put each line in it — something renders it
// unconditionally, and a `{#each}` key is rendered unconditionally even
// when nothing dereferences it.

import type { Event, FindingDetail, FindingOut, JobListEntry, Stats, Summary } from "./types";

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function isObjectArray(v: unknown): v is Record<string, unknown>[] {
  return Array.isArray(v) && v.every(isObject);
}

type KeyType = "number" | "string";

/**
 * A collection the UI renders with `{#each rows as row (row[key])}`.
 *
 * Svelte 5 throws `each_key_duplicate` on a repeated key instead of
 * dropping the row, and it throws inside the component — past the store,
 * where setting `error` could still have shown the operator a dashboard.
 * The page dies instead.
 *
 * Missing is the case that actually arrives: rows carrying none of the
 * field all key on the same `undefined`, so `[{}, {}]` is enough. That
 * is also why the type is checked and not just the count — `{}` and
 * `{ id: undefined }` are the same body and must get the same answer.
 *
 * The key type is a parameter because `StatsPage` keys `by_kind` by
 * `row.kind`, which is a string.
 */
function isKeyedList(v: unknown, key: string, type: KeyType): v is Record<string, unknown>[] {
  if (!isObjectArray(v)) return false;
  const seen = new Set<unknown>();
  for (const row of v) {
    if (typeof row[key] !== type) return false;
    seen.add(row[key]);
  }
  return seen.size === v.length;
}

/**
 * Each branch of the status panel reads one variant field with no `?.` —
 * `{@const cj = activity_status.job}` followed by `cj.kind` throws on a
 * body that claims `running` and carries no job. A kind nobody branches on
 * renders nothing, so it needs nothing.
 */
function hasActivityFields(a: Record<string, unknown>): boolean {
  switch (a.kind) {
    case "running":
      return isObject(a.job);
    case "paused":
    case "ready":
      return isObject(a.candidate);
    case "error":
    case "offline":
      return typeof a.detail === "string";
    default:
      return true;
  }
}

/** `activity_status.kind` drives the whole status panel's branch. */
export function isSummary(v: unknown): v is Summary {
  if (!isObject(v)) return false;
  if (!isObject(v.activity_status) || typeof v.activity_status.kind !== "string") return false;
  if (!hasActivityFields(v.activity_status)) return false;
  if (typeof v.backend_status_html !== "string") return false;
  if (!isObject(v.counts) || !isObject(v.type_counts)) return false;
  // Scheduler controls send the inverse of these flags; a missing value
  // could only ever repeat the disabled state instead of toggling it.
  if (
    typeof v.scheduler_paused !== "boolean"
    || typeof v.scheduler_overdrive !== "boolean"
  ) return false;
  // Keyed, not merely records: `ReposPage` renders
  // `{#each repos as repo (repo.id)}`. It also reads `r.url` and
  // `r.added_at` off every entry to build a repo's identity, and three
  // pages read `r.id`/`r.name` for their repo-name maps — none of them
  // optionally. `repos: [null]` would pass an array check, reach the
  // store, and throw inside a `$derived`.
  return isKeyedList(v.repos, "id", "number") && v.repos.every(hasKeyableRepoWork);
}

/** `StatsPage` maps `by_kind`/`by_finding` and reads `totals.jobs`. */
export function isStats(v: unknown): v is Stats {
  if (!isObject(v)) return false;
  if (!isObject(v.totals)) return false;
  // `by_kind` is keyed by the kind string; `by_finding` by `finding_id`,
  // which is a number here even though `jobs.finding_id` is nullable —
  // `stats_by_finding` selects `WHERE j.finding_id IS NOT NULL` and
  // groups by it, and the page renders it as the row's link text.
  return isKeyedList(v.by_kind, "kind", "string")
    && isKeyedList(v.by_finding, "finding_id", "number");
}

/** `FindingCard` renders a finding's timeline keyed by `ev.id`. */
function hasKeyableTimeline(finding: Record<string, unknown>): boolean {
  // Absent or null is how a finding with no events arrives, and the card
  // guards the whole block on it.
  if (finding.timeline === undefined || finding.timeline === null) return true;
  return isKeyedList(finding.timeline, "id", "number");
}

/**
 * An id list rendered with `{#each ids as id (id)}`: `LogPage`'s
 * produced-finding links, `FindingCard`'s follow-up links.
 */
function hasDistinctIds(row: Record<string, unknown>, key: string): boolean {
  const ids = row[key];
  // Absent rather than empty when the daemon predates the field, which
  // both readers already treat as `[]`.
  if (ids === undefined || ids === null) return true;
  // `.slice()`/`.length` are read unconditionally once it is present.
  if (!Array.isArray(ids)) return false;
  return new Set(ids).size === ids.length;
}

/**
 * `FindingCard` renders a finding's holds keyed by `hold.kind` (one active
 * hold per kind) and each hold's buttons keyed by action.
 */
function hasKeyableHolds(finding: Record<string, unknown>): boolean {
  // Absent when the daemon predates the field; the card reads it as `[]`.
  if (finding.holds === undefined || finding.holds === null) return true;
  return (
    isKeyedList(finding.holds, "kind", "string") &&
    finding.holds.every((h) => Array.isArray(h.actions) && hasDistinctIds(h, "actions"))
  );
}

/**
 * `RepoWork` renders a repo's holds and failing work keyed by `kind` (one
 * of each per kind) and each hold's buttons keyed by action.
 */
function hasKeyableRepoWork(repo: Record<string, unknown>): boolean {
  // Absent when the daemon predates the fields; the component reads `[]`.
  const holds = repo.holds === undefined || repo.holds === null
    || (isKeyedList(repo.holds, "kind", "string")
      && repo.holds.every((h) => Array.isArray(h.actions) && hasDistinctIds(h, "actions")));
  const failing = repo.failing === undefined || repo.failing === null
    || isKeyedList(repo.failing, "kind", "string");
  return holds && failing;
}

/** `FindingDetail` reads `detail.jobs.length` and keys each row by `job.id`. */
export function isFindingDetail(v: unknown): v is FindingDetail {
  if (!isObject(v)) return false;
  if (!isKeyedList(v.jobs, "id", "number")) return false;
  // Nullable by contract, so absent is fine, but a non-object non-null
  // would be read as a record by the PR block.
  return v.pr_state === null || v.pr_state === undefined || isObject(v.pr_state);
}

/**
 * `/api/findings`. Every page that lists findings — inbox, all-findings,
 * and each kanban column plus its suppressed and notes sections — keys
 * them by `finding.id`, and the kanban lists are filtered subsets, so
 * distinctness across the whole body is what they all need.
 */
export function isFindingList(v: unknown): v is FindingOut[] {
  return (
    isKeyedList(v, "id", "number") &&
    v.every((f) => hasKeyableTimeline(f) && hasDistinctIds(f, "follow_ups") && hasKeyableHolds(f))
  );
}

/** `/api/jobs`. `LogPage` keys the job table by `job.id`. */
export function isJobList(v: unknown): v is JobListEntry[] {
  return isKeyedList(v, "id", "number") && v.every((j) => hasDistinctIds(j, "produced_finding_ids"));
}

/** `/api/events`. `LogPage` keys the event table by `ev.id`. */
export function isEventList(v: unknown): v is Event[] {
  return isKeyedList(v, "id", "number");
}
