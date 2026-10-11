import { describe, expect, it } from "vitest";

import {
  isEventList, isFindingDetail, isFindingList, isJobList, isStats, isSummary,
} from "./validate";

// A body shaped like what the daemon actually serves, trimmed to the
// fields the validators require.
const summary = {
  backend_status_html: "<div></div>",
  counts: { new: 1 },
  type_counts: { bug: 1 },
  repos: [],
  last_cycle: null,
  cycle_running: false,
  scheduler_paused: false,
  scheduler_overdrive: false,
  current_job: null,
  next_candidate: null,
  scheduler_state: null,
  activity_status: { kind: "idle" },
};

describe("isSummary", () => {
  it("accepts a real summary", () => {
    expect(isSummary(summary)).toBe(true);
  });

  // The case that motivated the validator: an empty object is an object,
  // so the old category check passed it through to the status panel.
  it("rejects an empty object", () => {
    expect(isSummary({})).toBe(false);
  });

  it("rejects a summary whose activity_status is missing or unusable", () => {
    expect(isSummary({ ...summary, activity_status: undefined })).toBe(false);
    expect(isSummary({ ...summary, activity_status: null })).toBe(false);
    // Present but without the discriminant every branch in StatusPage reads.
    expect(isSummary({ ...summary, activity_status: {} })).toBe(false);
  });

  it("rejects missing or non-boolean scheduler controls", () => {
    expect(isSummary({ ...summary, scheduler_paused: undefined })).toBe(false);
    expect(isSummary({ ...summary, scheduler_paused: null })).toBe(false);
    expect(isSummary({ ...summary, scheduler_paused: "false" })).toBe(false);
    expect(isSummary({ ...summary, scheduler_overdrive: undefined })).toBe(false);
    expect(isSummary({ ...summary, scheduler_overdrive: null })).toBe(false);
    expect(isSummary({ ...summary, scheduler_overdrive: "false" })).toBe(false);
  });

  // StatusPage's branches read these with no `?.`, so a body that claims
  // the kind without the field renders straight into a throw.
  const variants = [
    ["running", "job", { id: 1, kind: "hunt", repo_name: "r" }],
    ["paused", "candidate", { kind: "fix", id: 2, label: "x" }],
    ["ready", "candidate", { kind: "fix", id: 2, label: "x" }],
    ["error", "detail", "clone failed"],
    ["offline", "detail", "github.com unreachable"],
  ] as const;

  it.each(variants)("requires %s to carry %s", (kind, field, value) => {
    expect(isSummary({ ...summary, activity_status: { kind, [field]: value } })).toBe(true);
    expect(isSummary({ ...summary, activity_status: { kind } })).toBe(false);
  });

  it("rejects a variant field of the wrong shape", () => {
    expect(isSummary({ ...summary, activity_status: { kind: "running", job: "hunt" } })).toBe(false);
    expect(isSummary({ ...summary, activity_status: { kind: "ready", candidate: [] } })).toBe(false);
    expect(isSummary({ ...summary, activity_status: { kind: "error", detail: null } })).toBe(false);
  });

  // The kinds whose branches render no variant field, plus a kind no
  // branch matches: all render without dereferencing anything.
  it.each(["working", "idle", "warming_up", "hibernating"])(
    "accepts a bare %s activity status",
    (kind) => {
      expect(isSummary({ ...summary, activity_status: { kind } })).toBe(true);
    },
  );

  it("rejects an array, which is also typeof object", () => {
    expect(isSummary([])).toBe(false);
  });

  // Server growth must not blank the dashboard.
  it("accepts a summary carrying fields the client does not know", () => {
    expect(isSummary({ ...summary, something_new: 42 })).toBe(true);
  });
});

describe("isStats", () => {
  const stats = { totals: { jobs: 0 }, by_kind: [], by_finding: [] };

  it("accepts real stats", () => {
    expect(isStats(stats)).toBe(true);
  });

  it("rejects an empty object", () => {
    expect(isStats({})).toBe(false);
  });

  it("rejects collections that are not arrays of records", () => {
    expect(isStats({ ...stats, by_kind: {} })).toBe(false);
    expect(isStats({ ...stats, by_finding: ["nope"] })).toBe(false);
  });

  // StatsPage keys `by_kind` by the kind STRING — the one collection in
  // the UI whose key is not a number.
  it("requires by_kind rows to carry a distinct kind string", () => {
    expect(isStats({ ...stats, by_kind: [{ kind: "hunt" }, { kind: "fix" }] })).toBe(true);
    expect(isStats({ ...stats, by_kind: [{ jobs: 1 }] })).toBe(false);
    expect(isStats({ ...stats, by_kind: [{ kind: "hunt" }, { kind: "hunt" }] })).toBe(false);
    expect(isStats({ ...stats, by_kind: [{ kind: 3 }] })).toBe(false);
  });

  it("requires by_finding rows to carry a distinct numeric finding_id", () => {
    expect(isStats({ ...stats, by_finding: [{ finding_id: 1 }, { finding_id: 2 }] })).toBe(true);
    expect(isStats({ ...stats, by_finding: [{ fingerprint: "fp" }] })).toBe(false);
    expect(isStats({ ...stats, by_finding: [{ finding_id: 1 }, { finding_id: 1 }] })).toBe(false);
    expect(isStats({ ...stats, by_finding: [{ finding_id: "1" }] })).toBe(false);
    // Null is not a legitimate value here even though `jobs.finding_id`
    // is nullable: `stats_by_finding` selects `WHERE j.finding_id IS NOT
    // NULL` and groups by it. Two of them would key one row twice.
    expect(isStats({
      ...stats,
      by_finding: [{ finding_id: null }, { finding_id: null }],
    })).toBe(false);
  });
});

describe("isFindingDetail", () => {
  it("accepts a detail with jobs and either shape of pr_state", () => {
    expect(isFindingDetail({ jobs: [], pr_state: null })).toBe(true);
    expect(isFindingDetail({ jobs: [], pr_state: { number: 1 } })).toBe(true);
    expect(isFindingDetail({ jobs: [] })).toBe(true);
  });

  // FindingDetail.svelte keys the job table by job.id.
  it("rejects a detail whose jobs holds a non-object", () => {
    expect(isFindingDetail({ jobs: [{ id: 1 }] })).toBe(true);
    expect(isFindingDetail({ jobs: [null] })).toBe(false);
    expect(isFindingDetail({ jobs: [1] })).toBe(false);
  });

  // A keyed each throws on `undefined` seen twice, so a body whose jobs
  // carry no id crashes the panel rather than rendering it short.
  it("rejects a detail whose jobs have no usable id", () => {
    expect(isFindingDetail({ jobs: [{ kind: "hunt" }] })).toBe(false);
    expect(isFindingDetail({ jobs: [{ id: null }] })).toBe(false);
    expect(isFindingDetail({ jobs: [{ id: "7" }] })).toBe(false);
    expect(isFindingDetail({ jobs: [{ id: 1 }, { kind: "fix" }] })).toBe(false);
  });

  // Svelte 5 throws on a repeated key too, not just a missing one.
  it("rejects a detail whose job ids repeat", () => {
    expect(isFindingDetail({ jobs: [{ id: 1 }, { id: 1 }] })).toBe(false);
    expect(isFindingDetail({ jobs: [{ id: 1 }, { id: 2 }, { id: 1 }] })).toBe(false);
    expect(isFindingDetail({ jobs: [{ id: 1 }, { id: 2 }] })).toBe(true);
  });

  // FindingDetail.svelte reads detail.jobs.length with no guard.
  it("rejects a detail whose jobs is missing or not an array", () => {
    expect(isFindingDetail({ pr_state: null })).toBe(false);
    expect(isFindingDetail({ jobs: null })).toBe(false);
    expect(isFindingDetail({ jobs: 3 })).toBe(false);
  });

  it("rejects a non-object body", () => {
    expect(isFindingDetail("ok")).toBe(false);
    expect(isFindingDetail(null)).toBe(false);
  });
});

// Every polled list is rendered by a keyed `{#each}`: findings by
// `finding.id` on the inbox, all-findings and kanban pages (the kanban
// columns and its suppressed/notes sections are filtered subsets of the
// same body), jobs and events by `id` in the log.
const keyedLists: [string, (v: unknown) => boolean][] = [
  ["isFindingList", isFindingList],
  ["isJobList", isJobList],
  ["isEventList", isEventList],
];

describe.each(keyedLists)("%s", (_name, isList) => {
  it("accepts an empty list and distinctly keyed records", () => {
    expect(isList([])).toBe(true);
    expect(isList([{ id: 1 }, { id: 2 }])).toBe(true);
  });

  it("rejects a non-array and a list holding non-records", () => {
    expect(isList({})).toBe(false);
    expect(isList([1, 2])).toBe(false);
    expect(isList([null])).toBe(false);
  });

  // The body that motivated the guard: two records with nothing in them
  // key the block on `undefined` twice.
  it("rejects rows with no id to key on", () => {
    expect(isList([{}, {}])).toBe(false);
    expect(isList([{ id: 1 }, { kind: "x" }])).toBe(false);
  });

  it("rejects a repeated id", () => {
    expect(isList([{ id: 1 }, { id: 1 }])).toBe(false);
    expect(isList([{ id: 1 }, { id: 2 }, { id: 1 }])).toBe(false);
  });

  it("rejects an id of the wrong type", () => {
    expect(isList([{ id: "7" }])).toBe(false);
    expect(isList([{ id: null }])).toBe(false);
  });
});

// FindingCard renders `{#each finding.timeline as ev (ev.id)}`, one
// level down inside the findings body.
describe("isFindingList timelines", () => {
  it("accepts a finding with no timeline, or a keyable one", () => {
    expect(isFindingList([{ id: 1 }])).toBe(true);
    expect(isFindingList([{ id: 1, timeline: null }])).toBe(true);
    expect(isFindingList([{ id: 1, timeline: [] }])).toBe(true);
    expect(isFindingList([{ id: 1, timeline: [{ id: 9 }, { id: 10 }] }])).toBe(true);
  });

  it("rejects a timeline the card cannot key", () => {
    expect(isFindingList([{ id: 1, timeline: [{}, {}] }])).toBe(false);
    expect(isFindingList([{ id: 1, timeline: [{ id: 9 }, { id: 9 }] }])).toBe(false);
    expect(isFindingList([{ id: 1, timeline: [{ id: "9" }] }])).toBe(false);
    // Truthy with a length, so the card enters the block and iterates it.
    expect(isFindingList([{ id: 1, timeline: "oops" }])).toBe(false);
  });
});

// LogPage links a hunt's output with
// `{#each produced(job).slice(0, 3) as fid (fid)}` — keyed by the value
// itself, and `.slice` is called on whatever arrived.
describe("isJobList produced_finding_ids", () => {
  it("accepts absent, empty and distinct id lists", () => {
    expect(isJobList([{ id: 1 }])).toBe(true);
    expect(isJobList([{ id: 1, produced_finding_ids: null }])).toBe(true);
    expect(isJobList([{ id: 1, produced_finding_ids: [] }])).toBe(true);
    expect(isJobList([{ id: 1, produced_finding_ids: [4, 5] }])).toBe(true);
  });

  it("rejects a repeated id and a non-array", () => {
    expect(isJobList([{ id: 1, produced_finding_ids: [4, 4] }])).toBe(false);
    expect(isJobList([{ id: 1, produced_finding_ids: 4 }])).toBe(false);
  });
});

// FindingCard renders `{#each followUps as fid (fid)}`.
describe("isFindingList follow_ups", () => {
  it("accepts absent, empty and distinct id lists", () => {
    expect(isFindingList([{ id: 1 }])).toBe(true);
    expect(isFindingList([{ id: 1, follow_ups: [] }])).toBe(true);
    expect(isFindingList([{ id: 1, follow_ups: [4, 5] }])).toBe(true);
  });

  it("rejects a repeated id and a non-array", () => {
    expect(isFindingList([{ id: 1, follow_ups: [4, 4] }])).toBe(false);
    expect(isFindingList([{ id: 1, follow_ups: 4 }])).toBe(false);
  });
});

// FindingCard renders `{#each holds as hold (hold.kind)}` and, per hold,
// `{#each hold.actions as action (action)}`.
describe("isFindingList holds", () => {
  const hold = (kind: unknown, actions: unknown) => ({ kind, actions });

  it("accepts absent, empty and one hold per kind", () => {
    expect(isFindingList([{ id: 1 }])).toBe(true);
    expect(isFindingList([{ id: 1, holds: [] }])).toBe(true);
    expect(
      isFindingList([{ id: 1, holds: [hold("fix", ["continue", "restart"]), hold("engage", [])] }]),
    ).toBe(true);
  });

  it("rejects a repeated kind, a missing kind, and repeated or missing actions", () => {
    expect(isFindingList([{ id: 1, holds: [hold("fix", []), hold("fix", [])] }])).toBe(false);
    expect(isFindingList([{ id: 1, holds: [{ actions: [] }] }])).toBe(false);
    expect(isFindingList([{ id: 1, holds: [hold("fix", ["continue", "continue"])] }])).toBe(false);
    expect(isFindingList([{ id: 1, holds: [{ kind: "fix" }] }])).toBe(false);
  });
});

describe("isSummary repos entries", () => {
  it("accepts an empty list and a list of records", () => {
    expect(isSummary({ ...summary, repos: [] })).toBe(true);
    expect(isSummary({ ...summary, repos: [{ id: 1, name: "a" }] })).toBe(true);
  });

  // ReposPage reads r.url/r.added_at, and three pages read r.id/r.name,
  // none of them optionally — an array of non-records throws in a
  // $derived rather than setting the API error state.
  it("rejects entries that are not records", () => {
    expect(isSummary({ ...summary, repos: [null] })).toBe(false);
    expect(isSummary({ ...summary, repos: [1] })).toBe(false);
    expect(isSummary({ ...summary, repos: [{ id: 1 }, "nope"] })).toBe(false);
  });

  // ReposPage renders `{#each repos as repo (repo.id)}`.
  it("rejects repos the page cannot key by id", () => {
    expect(isSummary({ ...summary, repos: [{ name: "a" }, { name: "b" }] })).toBe(false);
    expect(isSummary({ ...summary, repos: [{ id: 1 }, { id: 1 }] })).toBe(false);
    expect(isSummary({ ...summary, repos: [{ id: "1" }] })).toBe(false);
  });
});
