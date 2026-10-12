// Shared formatting utilities (ported from app.ts helpers).

/** Token count to human-readable: 150000 → "150k", 2100000 → "2.1M" */
export function ktok(n: number | null): string {
  if (n == null) return "\u2013";
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
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
