import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { clock, datetime, isHttpUrl, ktok, reviewStateLabel, ts } from "./format";

describe("reviewStateLabel", () => {
  it("names a review that ended on hunter's own PR", () => {
    expect(reviewStateLabel("own")).toBe("hunter's own PR");
  });

  it.each(["open", "merged", "closed"])("shows %s as is", (s) => {
    expect(reviewStateLabel(s)).toBe(s);
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
