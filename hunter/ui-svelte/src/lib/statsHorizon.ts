// The Stats page's time horizon (GET /api/stats?horizon=), remembered per
// browser across reloads.

import type { StatsHorizon } from "./types";

/**
 * Every horizon the daemon accepts, in display order, with its button
 * label and the window it describes. A `Record` over the generated union
 * so a horizon added in hunter-rs fails the type check here until it is
 * given both.
 */
export const STATS_HORIZONS: Record<StatsHorizon, { label: string; window: string }> = {
  "24h": { label: "24h", window: "Jobs started in the last 24 hours" },
  "7d": { label: "7d", window: "Jobs started in the last 7 days" },
  "30d": { label: "30d", window: "Jobs started in the last 30 days" },
  "90d": { label: "90d", window: "Jobs started in the last 90 days" },
  all: { label: "All", window: "All jobs" },
};

const STORAGE_KEY = "hunter.stats.horizon";

function isStatsHorizon(v: unknown): v is StatsHorizon {
  return typeof v === "string" && Object.hasOwn(STATS_HORIZONS, v);
}

// Storage can be unavailable (disabled, sandboxed iframe, a test runtime
// without it) and then throws on access. The preference is a convenience:
// without storage the page still works, it just opens at `all` again.

/** The remembered horizon, or `all` when none (valid) is stored. */
export function loadStatsHorizon(): StatsHorizon {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    return isStatsHorizon(stored) ? stored : "all";
  } catch {
    return "all";
  }
}

export function saveStatsHorizon(horizon: StatsHorizon): void {
  try {
    localStorage.setItem(STORAGE_KEY, horizon);
  } catch {
    // See above: not remembered, nothing else lost.
  }
}
