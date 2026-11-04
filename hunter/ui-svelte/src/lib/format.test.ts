import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { clock, datetime, isHttpUrl, ktok, reviewIssueStatusLabel, reviewStateLabel, roundPostTally, roundWait, ts } from "./format";
import type { PrReviewOut } from "./types";

describe("reviewStateLabel", () => {
  it("names a review that ended on hunter's own PR", () => {
    expect(reviewStateLabel("own")).toBe("hunter's own PR");
  });

  it.each(["open", "merged", "closed"])("shows %s as is", (s) => {
    expect(reviewStateLabel(s)).toBe(s);
  });
});

describe("reviewIssueStatusLabel", () => {
  it("says a rebutted issue's rebuttal was accepted", () => {
    expect(reviewIssueStatusLabel("rebutted")).toBe("rebuttal accepted");
  });

  it.each(["open", "addressed", "reopened"])("shows %s as is", (s) => {
    expect(reviewIssueStatusLabel(s)).toBe(s);
  });
});

/** The Post button's count of what posting a round sends. */
describe("roundPostTally", () => {
  it("counts issues alone when the round resolves nothing", () => {
    expect(roundPostTally(1, 0)).toBe("1 issue");
    expect(roundPostTally(2, 0)).toBe("2 issues");
  });

  it("adds the resolutions after the issues", () => {
    expect(roundPostTally(2, 1)).toBe("2 issues, 1 resolution");
    expect(roundPostTally(1, 3)).toBe("1 issue, 3 resolutions");
  });

  it("names only the resolutions of a round without new issues", () => {
    expect(roundPostTally(0, 1)).toBe("1 resolution");
    expect(roundPostTally(0, 2)).toBe("2 resolutions");
  });
});

/** Whether a review shows as waiting for a round is the daemon's `due_at`. */
describe("roundWait", () => {
  const now = 1_700_000_000_000;
  const review = (extra: Partial<PrReviewOut>) => ({
    watch: true, head_sha: "b", last_reviewed_sha: "a", running_job_id: null, due_at: null, rounds: [], ...extra,
  }) as PrReviewOut;

  it("shows an unwatched review a request asked a round of as pending", () => {
    const wait = roundWait(review({ watch: false, due_at: 0 }), now);
    expect(wait).toEqual({ label: "pending", title: "Waiting for the scheduler" });
  });

  it("shows no badge while no round is due, as for a review whose repo moved", () => {
    expect(roundWait(review({ due_at: null }), now)).toBeNull();
    expect(roundWait(review({ watch: false, last_reviewed_sha: null, due_at: null }), now)).toBeNull();
  });

  it("shows no badge while the round runs", () => {
    expect(roundWait(review({ due_at: 0, running_job_id: 3 }), now)).toBeNull();
  });

  it("says a round due later waits for the head to stay unchanged", () => {
    expect(roundWait(review({ due_at: now + 60_000 }), now)?.title).toMatch(/^Waiting for the head to stay unchanged until /);
  });
});

/**
 * Repo URLs are operator-supplied and rendered as links. Only http(s)
 * navigates; everything else is shown as text. This covers rows stored
 * before the write path validated schemes.
 */
describe("isHttpUrl", () => {
  it.each([
    "javascript:alert(1)",
    "JavaScript:alert(1)",
    "data:text/html,<script>alert(1)</script>",
    "vbscript:msgbox(1)",
  ])("refuses to link %s", (u) => {
    expect(isHttpUrl(u)).toBe(false);
  });

  it.each(["https://github.com/acme/widget.git", "http://git.internal/acme/widget.git"])(
    "links %s",
    (u) => {
      expect(isHttpUrl(u)).toBe(true);
    },
  );

  it("shows an ssh clone URL as text rather than a link", () => {
    // Not dangerous, but not navigable either.
    expect(isHttpUrl("git@github.com:acme/widget.git")).toBe(false);
  });
});

/**
 * The budget ETA is read at a glance: a bare "8:00 AM" that is really
 * tomorrow's reads as a few hours away. The date goes in exactly when the
 * instant is not on today's local calendar date, however close it is.
 */
describe("clock", () => {
  // Local-time constructors, so the boundary is local midnight in any TZ.
  const now = new Date(2026, 9, 9, 23, 58).getTime();

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(now);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("shows only the time for the last minute of today", () => {
    const t = new Date(2026, 9, 9, 23, 59).getTime();
    expect(clock(t)).toBe(ts(t));
  });

  it("dates the first minute of tomorrow", () => {
    const t = new Date(2026, 9, 10, 0, 0).getTime();
    expect(clock(t)).toBe(datetime(t));
  });

  it("dates the same clock time on another day", () => {
    const t = new Date(2026, 9, 10, 23, 58).getTime();
    expect(clock(t)).toBe(datetime(t));
  });

  it("shows a dash when there is no instant", () => {
    expect(clock(null)).toBe("\u2013");
  });
});

/** Counts that round up to 1000k belong to the M branch. */
describe("ktok", () => {
  it.each([
    [999_499, "999k"],
    [999_500, "1.0M"],
    [999_999, "1.0M"],
    [1_000_000, "1.0M"],
  ])("formats %d as %s", (n, want) => {
    expect(ktok(n)).toBe(want);
  });
});
