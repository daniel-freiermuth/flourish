// Shared formatting utilities (ported from app.ts helpers).

import type { PrReviewOut } from "./types";

/** Token count to human-readable: 150000 → "150k", 2100000 → "2.1M" */
export function ktok(n: number | null): string {
  if (n == null) return "\u2013";
  if (n >= 999_500) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1_000) return Math.round(n / 1_000) + "k";
  return String(n);
}

// Built once: `toLocale*String` with an options object constructs a fresh
// Intl.DateTimeFormat on every call (~65µs in Chromium), and a findings page
// formats one date per timeline row — 13k rows cost 1.7s per render.
const TIME_FMT = new Intl.DateTimeFormat([], { hour: "2-digit", minute: "2-digit" });
const DAY_FMT = new Intl.DateTimeFormat([], { month: "short", day: "numeric" });

/** Epoch ms → local time string "2:05 PM" */
export function ts(ms: number | null): string {
  if (!ms) return "\u2013";
  return TIME_FMT.format(ms);
}

/** Epoch ms → "Sep 14 2:05 PM" */
export function datetime(ms: number | null): string {
  if (!ms) return "\u2013";
  return DAY_FMT.format(ms) + " " + TIME_FMT.format(ms);
}

/** Epoch ms → "2:05 PM" when it falls on today's local date, else "Sep 14 2:05 PM". */
export function clock(ms: number | null): string {
  if (!ms) return "\u2013";
  return new Date(ms).toDateString() === new Date().toDateString() ? ts(ms) : datetime(ms);
}

/** Epoch ms → countdown "2h05m" / "45m" / "-3m" */
export function countdown(ms: number | null): string {
  if (!ms) return "";
  let d = Math.round((ms - Date.now()) / 1000);
  const sign = d < 0 ? "-" : "";
  d = Math.abs(d);
  const h = Math.floor(d / 3600);
  const m = Math.floor((d % 3600) / 60);
  return h > 0 ? `${sign}${h}h${String(m).padStart(2, "0")}m` : `${sign}${m}m`;
}

/** Job duration in seconds. */
export function dur(startedAt: number | null, finishedAt: number | null): string {
  if (!startedAt) return "\u2013";
  const end = finishedAt || Date.now();
  return Math.round((end - startedAt) / 1000) + "s";
}

/** Percentage formatting: 0.76 → "76.00%" */
export function pct(v: number | null): string {
  return v != null ? (v * 100).toFixed(2) + "%" : "\u2013";
}

/** HTML-escape for safe interpolation. */
export function esc(s: unknown): string {
  const map: Record<string, string> = {
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  };
  return String(s ?? "").replace(/[&<>"']/g, (c) => map[c] ?? c);
}

/** Severity rank for sorting. */
export const SEV_RANK: Record<string, number> = { high: 3, medium: 2, low: 1 };

/** Severity CSS color class. */
export function sevColor(sev: string): string {
  switch (sev) {
    case "high":
      return "text-sev-high";
    case "medium":
      return "text-sev-medium";
    case "low":
      return "text-sev-low";
    default:
      return "text-text-dim";
  }
}

/** Type emoji + short label (matches original UI). */
export function typeLabel(t: string): string {
  switch (t) {
    case "bug":
      return "\ud83d\udc1b Bug";
    case "dep_update":
      return "\ud83d\udce6 Dep";
    case "test_gap":
      return "\ud83e\uddea Test";
    case "refactor":
      return "\u267b\ufe0f Refactor";
    case "modernization":
      return "\ud83d\udd2c Modern";
    case "standards":
      return "\ud83d\udcd0 Standards";
    default:
      return t || "?";
  }
}

/** Type emoji only. */
export function typeEmoji(t: string): string {
  switch (t) {
    case "bug":
      return "\ud83d\udc1b";
    case "dep_update":
      return "\ud83d\udce6";
    case "test_gap":
      return "\ud83e\uddea";
    case "refactor":
      return "\u267b\ufe0f";
    case "modernization":
      return "\ud83d\udd2c";
    case "standards":
      return "\ud83d\udcd0";
    default:
      return "\u2753";
  }
}

/** Why a finding's or a repo's work is held for the operator (`HoldReason`). */
export function holdReasonLabel(r: string): string {
  switch (r) {
    case "prerequisite":
      return "missing prerequisite";
    case "stuck":
      return "kept failing";
    case "gave_up":
      return "gave up";
    default:
      return r || "?";
  }
}

/**
 * A repo's work item (`RepoWorkKind`): a scan by its job kind, or the
 * repo's sync, whose failures stop every scan of the repo.
 */
export function repoWorkLabel(k: string): string {
  return k === "sync" ? "sync (clone, fetch, branch)" : k || "?";
}

/**
 * A job kind where the status panel names it. Kind strings read as words
 * except the PR review's, whose snake_case string would be the only one
 * on the panel that does not.
 */
export function jobKindLabel(kind: string): string {
  return kind === "pr_review" ? "PR review" : kind;
}

/**
 * A PR review's state on its badge: its PR's while the review follows it
 * (open, merged, closed). `own` is a review that ended because hunter
 * opened the PR itself, which is engaged, never reviewed.
 */
export function reviewStateLabel(s: string): string {
  return s === "own" ? "hunter's own PR" : s || "?";
}

/** What a review waiting for its next round shows: its badge and why. */
export interface RoundWait {
  label: string;
  title: string;
}

/**
 * The badge of a review waiting for its next round, or null when no round
 * is due or one is running. When a round is due is the daemon's call
 * (`due_at`, the rule its scheduler picks by), never worked out here: an
 * unwatched review a request asked a round of is due, one whose repo
 * moved or whose fork changed hands never is. A round due later waits for
 * the head to stay unchanged. At the reviewed head only news in the
 * review's threads makes a round due: the issues it judges again, once
 * the newest news has been quiet a while, or at once when a request asked.
 */
export function roundWait(review: PrReviewOut, now: number): RoundWait | null {
  if (review.due_at === null || review.running_job_id !== null) return null;
  if (review.head_sha === review.last_reviewed_sha) {
    const n = issuesAwaitingJudgment(review);
    return {
      label: `${n === 1 ? "1 issue" : `${n} issues`} to judge again \u2014 review queued`,
      title: review.due_at <= now
        ? "Replies or reopened threads that no round has judged: waiting for the scheduler"
        : "Replies or reopened threads that no round has judged: a round judges them once the newest has been quiet for two minutes",
    };
  }
  return {
    label: "pending",
    title: review.due_at <= now
      ? "Waiting for the scheduler"
      : `Waiting for the head to stay unchanged until ${clock(review.due_at)}, then for the scheduler`,
  };
}

/** The button for a way to release a hold (`ReleaseAction`). */
export function releaseActionLabel(a: string): string {
  switch (a) {
    case "continue":
      return "Continue";
    case "restart":
      return "Restart";
    case "reject":
      return "Reject";
    case "wontfix":
      return "Wontfix";
    case "classify":
      return "Classify";
    case "mark_harvested":
      return "Mark harvested";
    case "dismiss":
      return "Dismiss";
    default:
      return a || "?";
  }
}

/**
 * A PR review issue's status as later rounds judged it. `rebutted` means
 * the reviewer accepted a reply's rebuttal, not that the issue was
 * argued against, so it says so.
 */
export function reviewIssueStatusLabel(s: string): string {
  return s === "rebutted" ? "rebuttal accepted" : s || "?";
}

/**
 * What posting a review round sends: its issues, the replies that resolve
 * earlier issues' threads, and the answers to replies in the threads of
 * issues it found still open. A round without new issues names only what
 * it does post rather than reading as "0 issues", which would suggest
 * nothing posts.
 */
export function roundPostTally(issues: number, resolutions: number, answers: number): string {
  const parts: string[] = [];
  if (issues > 0 || (resolutions === 0 && answers === 0)) {
    parts.push(`${issues} ${issues === 1 ? "issue" : "issues"}`);
  }
  if (resolutions > 0) parts.push(`${resolutions} ${resolutions === 1 ? "resolution" : "resolutions"}`);
  if (answers > 0) parts.push(`${answers} ${answers === 1 ? "answer" : "answers"}`);
  return parts.join(", ");
}

/**
 * How many issues of a review await judgment: their thread holds a reply
 * newer than any their newest verdict was shown, or a human reopened it
 * since. What makes a watched review due without new commits.
 */
export function issuesAwaitingJudgment(review: PrReviewOut): number {
  let n = 0;
  for (const round of review.rounds) {
    n += round.issues.filter((i) => i.awaits_judgment).length;
  }
  return n;
}

/**
 * Is this safe to put in an `href`?
 *
 * Only http(s) navigates somewhere a browser should follow. Anything
 * else — `javascript:` above all, but also the ssh clone URLs the daemon
 * legitimately stores — is rendered as text instead. The write path
 * rejects dangerous schemes too; this is the half that also covers rows
 * stored before it did.
 */
export function isHttpUrl(url: string): boolean {
  try {
    const p = new URL(url).protocol;
    return p === "http:" || p === "https:";
  } catch {
    return false; // not absolute (scp-like ssh URL, bare path)
  }
}
