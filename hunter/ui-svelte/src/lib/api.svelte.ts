// Reactive API store — polls every 5s, exposes Svelte 5 rune state
// via a singleton store object.

import type {
  Event, FindingDetail, FindingOut, JobListEntry, RepoNotesResponse, SessionUser, Stats,
  StatsHorizon, Summary,
} from "./types";
import { loadStatsHorizon, saveStatsHorizon } from "./statsHorizon";
import {
  isEventList, isFindingDetail, isFindingList, isJobList, isStats, isSummary,
} from "./validate";

/** Dashboard poll period. Exported so a test can drive the timer by it. */
export const POLL_MS = 5000;

// ---------------------------------------------------------------------------
// Fetch wrapper
// ---------------------------------------------------------------------------

interface ApiResult<T> {
  status: number;
  /**
   * 2xx. Prefer this over comparing `status` to a literal: the write
   * endpoints are a mix of 200 and 201 (see hunter-rs/API-CONTRACT-WRITES.md
   * §7), and guessing which is which has already shipped as a bug — a 201
   * from /api/repo/notes read as failure. Compare `status` only where the
   * code carries meaning the caller acts on, such as 202 vs 409 on
   * /api/cycle.
   */
  ok: boolean;
  body: T | null;
}

/**
 * The one endpoint that answers 401 for a reason other than "no session":
 * there it means wrong credentials, and `login()` reports it.
 */
const LOGIN_PATH = "/api/login";

async function api<T>(path: string, opts?: RequestInit): Promise<ApiResult<T>> {
  // Captured before the request leaves: a 401 answers for the session the
  // request was sent under, and must not end one established since.
  const epoch = store.sessionEpoch;
  const r = await fetch(path, opts);
  // Every route but login requires the session cookie, so a 401 from any
  // of them — poll, detail fetch or write — means the session is gone
  // (expired, logged out in another tab, user removed). Handled here once
  // rather than in each of the components that call `post`.
  if (r.status === 401 && path !== LOGIN_PATH) store.sessionLost(epoch);
  let body: T | null = null;
  try {
    body = (await r.json()) as T;
  } catch (err) {
    // Empty body or non-JSON — unless the read was aborted. The deadline
    // below covers the body too, and a connection that died halfway
    // through 3MB of findings reported as an unparseable 200 would send
    // the operator hunting a malformed response nobody ever sent.
    if (opts?.signal?.aborted) throw err;
  }
  return { status: r.status, ok: r.ok, body };
}

/**
 * Upper bound on a read, in ms. Exported so a test can drive the clock by it.
 *
 * A GET has to be bounded because nothing else bounds it: `#poll()` skips
 * every tick while a refresh is in flight, so a daemon that accepts the
 * connection and then answers nothing wedges the gate shut for good, with
 * `error` still null — App.svelte then shows frozen data with no staleness
 * banner, forever. The rejection here is what reopens the gate and lights
 * the banner.
 *
 * 20s = 4 poll periods. The floor is generosity: the slowest read is
 * /api/findings, ~3.2MB for the 689 findings held today, served in ~85ms
 * on loopback, and a refresh that merely outlasts a few ticks (daemon
 * mid-cycle, five requests queued behind the browser's per-origin
 * connection limit) must still be allowed to land — the store promises
 * exactly that. The ceiling is the cost of being wrong: this bound plus
 * one poll period is how long the dashboard can present stale data as
 * live, so ~25s worst case.
 *
 * Deadline built from the global timer rather than `AbortSignal.timeout`,
 * which the runtime does support: that one runs on an internal clock
 * nothing can advance, so the bound would be unobservable from the same
 * clock that drives POLL_MS.
 */
export const GET_TIMEOUT_MS = 20_000;

/**
 * GET with a deadline. Writes deliberately do not get one — see `post`.
 */
async function get<T>(path: string): Promise<ApiResult<T>> {
  const deadline = new AbortController();
  const timer = setTimeout(
    () => deadline.abort(new Error(`GET ${path} timed out after ${GET_TIMEOUT_MS}ms`)),
    GET_TIMEOUT_MS,
  );
  try {
    return await api<T>(path, { signal: deadline.signal });
  } finally {
    clearTimeout(timer);
  }
}

/**
 * POST with JSON body, Content-Type set.
 *
 * No deadline, unlike `get`. Three reasons, in order of weight:
 * a write is user-initiated and its outcome is displayed (button text,
 * toast, console error), so a hang is visible and retryable, where a
 * hung poll is silent and self-blocking; aborting a write does not
 * cancel the server-side work, it only discards the answer, leaving the
 * caller unable to tell "applied" from "refused" on a request that may
 * already have been applied; and nothing here can estimate how long a
 * write should take — POST /api/repo/delete removes the repo's clone
 * inline, which is as slow as the checkout is large. Nothing queues
 * behind a write either: a mutation refresh is not gated, so a stuck
 * one cannot stop the poll.
 */
export async function post<T>(path: string, body: Record<string, unknown>): Promise<ApiResult<T>> {
  return api<T>(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

/**
 * `post` with `get`'s deadline, for login and logout only. The reasons
 * `post` has none do not hold for them: their buttons stay disabled while
 * the request is pending, so a hang is not retryable but a dead end; and
 * an abandoned attempt is harmless either way -- an unused session row
 * expires, and a logout the client gave up on is retried by clicking
 * again, or settled by the next poll's 401.
 */
async function postWithDeadline<T>(path: string, body: Record<string, unknown>): Promise<ApiResult<T>> {
  const deadline = new AbortController();
  const timer = setTimeout(
    () => deadline.abort(new Error(`POST ${path} timed out after ${GET_TIMEOUT_MS}ms`)),
    GET_TIMEOUT_MS,
  );
  try {
    return await api<T>(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal: deadline.signal,
    });
  } finally {
    clearTimeout(timer);
  }
}

// ---------------------------------------------------------------------------
// Store singleton (Svelte 5 runes inside a class)
// ---------------------------------------------------------------------------

/** The `{"error": "..."}` body every failing endpoint answers with, if it is one. */
function errorMessage(body: unknown): string | null {
  if (typeof body !== "object" || body === null) return null;
  const msg = (body as Record<string, unknown>).error;
  return typeof msg === "string" ? msg : null;
}

class HunterStore {
  summary = $state<Summary | null>(null);
  findings = $state<FindingOut[]>([]);
  jobs = $state<JobListEntry[]>([]);
  events = $state<Event[]>([]);
  stats = $state<Stats | null>(null);
  error = $state<string | null>(null);

  /**
   * Window `stats` is aggregated over; every refresh asks for it. A
   * per-browser preference, so it outlives a session change.
   */
  statsHorizon = $state<StatsHorizon>(loadStatsHorizon());

  /** Account the session belongs to; null while not (yet) known. */
  user = $state<string | null>(null);
  /**
   * The daemon refused the session (or there never was one): App.svelte
   * shows the login form instead of the dashboard, and nothing polls.
   */
  needsLogin = $state(false);

  // Bumped whenever the session changes hands (login, session end). See
  // `api()`: a 401 is only believed for the epoch its request was sent in.
  #sessionEpoch = 0;

  // Pending retry of start()'s session check after a failure that was not
  // a 401 (daemon down, 5xx).
  #startRetry: ReturnType<typeof setTimeout> | null = null;

  // Lazy caches (not reactive — components re-fetch on demand). Detail
  // responses are tagged with the data revision they were read in, see
  // fetchFindingDetail.
  findingDetailCache = new Map<number, { revision: number; detail: FindingDetail }>();
  repoNotesCache = new Map<number, string>();

  #interval: ReturnType<typeof setInterval> | null = null;

  // Guards against a stale in-flight refresh clobbering a newer one when the
  // poll timer and a mutation handler overlap: only the latest generation may
  // write state.
  #refreshGeneration = 0;

  // Set for the duration of a *poll* refresh only. The timer fires every
  // POLL_MS whether or not the previous tick came back, so once a refresh
  // outlasts the interval — slow daemon, or five requests queued behind the
  // browser's per-origin connection limit — each tick supersedes the one
  // still running and every result is dropped by the generation check. The
  // dashboard then freezes with `error` still null, which is the worst
  // failure available: App.svelte's "may be stale" banner keys off `error`,
  // so frozen data reads as live. A mutation refresh is deliberately not
  // gated — a user action must supersede a slow poll, never queue behind it.
  #pollInFlight = false;

  // Bumped only where the polled state above is actually replaced, so a
  // superseded or failing refresh neither invalidates caches nor wakes
  // subscribers. Nothing on the read path calls refresh(), so observing this
  // cannot feed back into a refresh loop.
  #revision = $state(0);

  async refresh() {
    // Every request would answer 401; the login form is up already.
    if (this.needsLogin) return;
    const gen = ++this.#refreshGeneration;
    try {
      const [s, f, j, e, st] = await Promise.all([
        get<Summary>("/api/summary"),
        get<FindingOut[]>("/api/findings"),
        get<JobListEntry[]>("/api/jobs"),
        get<Event[]>("/api/events"),
        get<Stats>(`/api/stats?horizon=${this.statsHorizon}`),
      ]);
      if (gen !== this.#refreshGeneration) return;
      if ([s, f, j, e, st].some((r) => r.status !== 200)) {
        this.error = "API error (non-200 response)";
        return;
      }
      // A 200 whose body failed to parse (truncated response, proxy error
      // page, Content-Type mismatch) arrives as null, and a body that parsed
      // to the wrong shape is no more usable. Reject before touching any
      // state: writing these through would swap real data for a confident
      // empty dashboard, and bumping #revision would send every open detail
      // panel refetching against data that never landed.
      //
      // Checking the category alone was not enough to keep that promise:
      // `{}` is an object, so it passed as a Summary and the status panel
      // then dereferenced its missing `activity_status`. Nor is
      // record-ness enough for the lists: every one of them is rendered
      // by a keyed `{#each}`, which throws on a missing or repeated key
      // inside the component, where this `error` no longer reaches.
      if (
        !isSummary(s.body)
        || !isStats(st.body)
        || !isFindingList(f.body)
        || !isJobList(j.body)
        || !isEventList(e.body)
      ) {
        this.error = "API error (malformed response body)";
        return;
      }
      this.summary = s.body;
      this.findings = f.body;
      this.jobs = j.body;
      this.events = e.body;
      this.stats = st.body;
      this.error = null;
      this.#revision++;
    } catch (err) {
      if (gen !== this.#refreshGeneration) return;
      this.error = String(err);
    }
  }

  /**
   * Switch the stats window, remember it, and refetch at once rather than
   * on the next tick. The stats on screen describe the old window, so they
   * go: the page shows "loading" until the new ones land. A refresh still
   * in flight was sent with the old horizon; the new one supersedes it
   * through the generation check in `refresh`.
   */
  setStatsHorizon(horizon: StatsHorizon) {
    if (horizon === this.statsHorizon) return;
    this.statsHorizon = horizon;
    saveStatsHorizon(horizon);
    this.stats = null;
    void this.refresh();
  }

  async #poll() {
    if (this.#pollInFlight) return;
    this.#pollInFlight = true;
    try {
      await this.refresh();
    } finally {
      this.#pollInFlight = false;
    }
  }

  startPolling() {
    this.#poll();
    this.#ensureInterval();
  }

  #ensureInterval() {
    if (!this.#interval) {
      this.#interval = setInterval(() => this.#poll(), POLL_MS);
    }
  }

  stopPolling() {
    if (this.#interval) {
      clearInterval(this.#interval);
      this.#interval = null;
    }
    if (this.#startRetry) {
      clearTimeout(this.#startRetry);
      this.#startRetry = null;
    }
  }

  get sessionEpoch(): number {
    return this.#sessionEpoch;
  }

  /**
   * Entry point for App.svelte: find out whether the browser holds a live
   * session before polling anything. A 401 raises the login form (via
   * `api()`); any other failure is shown like a failed poll and retried on
   * the poll period, so a daemon that was down at page load is picked up
   * once it is back, as polling used to.
   */
  async start() {
    this.stopPolling();
    // A login or logout while this is in flight makes its answer about a
    // session that no longer exists.
    const epoch = this.#sessionEpoch;
    try {
      const r = await get<SessionUser>("/api/me");
      if (this.needsLogin || epoch !== this.#sessionEpoch) return;
      if (r.ok) {
        this.user = typeof r.body?.username === "string" ? r.body.username : null;
        this.startPolling();
        return;
      }
      this.error = `API error (GET /api/me: status ${r.status})`;
    } catch (err) {
      this.error = String(err);
    }
    this.#startRetry = setTimeout(() => this.start(), POLL_MS);
  }

  /**
   * Sign in. Resolves to null on success — the dashboard then polls again
   * and refreshes at once — or to the message to show under the form.
   */
  async login(username: string, password: string): Promise<string | null> {
    let r: ApiResult<SessionUser>;
    try {
      r = await postWithDeadline<SessionUser>(LOGIN_PATH, { username, password });
    } catch (err) {
      return `Cannot reach the server (${String(err)})`;
    }
    if (!r.ok) return errorMessage(r.body) ?? `Login failed (status ${r.status})`;
    this.#sessionEpoch++;
    this.user = typeof r.body?.username === "string" ? r.body.username : username;
    this.needsLogin = false;
    this.error = null;
    // refresh() rather than startPolling(): a poll left over from the
    // previous session may still hold the gate, and the first data must
    // not wait for it.
    this.#ensureInterval();
    void this.refresh();
    return null;
  }

  /**
   * End the session on the server, then locally. Resolves to null, or to
   * a message when the server could not be told — the session cookie is
   * still live then, so the dashboard stays rather than pretending.
   */
  async logout(): Promise<string | null> {
    let r: ApiResult<unknown>;
    try {
      r = await postWithDeadline<unknown>("/api/logout", {});
    } catch (err) {
      return `Logout failed (${String(err)})`;
    }
    // A 401 means the session was already gone; api() has handled it.
    if (r.ok) this.#endSession();
    else if (r.status !== 401) return errorMessage(r.body) ?? `Logout failed (status ${r.status})`;
    return null;
  }

  /** Called by `api()` on a 401 from a request sent in session `epoch`. */
  sessionLost(epoch: number) {
    if (epoch !== this.#sessionEpoch || this.needsLogin) return;
    this.#endSession();
  }

  #endSession() {
    this.stopPolling();
    this.#sessionEpoch++;
    // Discards any refresh still in flight: its answer belongs to the
    // session that just ended.
    this.#refreshGeneration++;
    this.user = null;
    this.needsLogin = true;
    // Nothing read under the old session may outlive it on screen — the
    // next login may be someone else's.
    this.summary = null;
    this.findings = [];
    this.jobs = [];
    this.events = [];
    this.stats = null;
    this.error = null;
    this.findingDetailCache.clear();
    this.repoNotesCache.clear();
  }

  /**
   * Version of the polled data. Reactive: read it inside an `$effect` to
   * re-run when — and only when — a refresh landed new data.
   */
  get revision(): number {
    return this.#revision;
  }

  async fetchFindingDetail(id: number): Promise<FindingDetail | null> {
    // Tying the cache to the data revision rather than dropping it keeps
    // open/close churn free while guaranteeing the panel never shows job or PR
    // state older than the last 5s poll — a TTL would need its own clock and
    // would still drift from the poll that changed the data.
    const cached = this.findingDetailCache.get(id);
    if (cached && cached.revision === this.#revision) return cached.detail;
    // Captured BEFORE the await: a poll can land while this request is in
    // flight, and tagging the response with the revision current at
    // write time would file data fetched against the old state as though
    // it were the new one — the refresh-triggered reload would then be
    // served this stale body from cache and never see the change.
    const requestedAt = this.#revision;
    const epoch = this.#sessionEpoch;
    const r = await get<FindingDetail>(`/api/finding?id=${id}`);
    // Read under a session that has since ended: neither shown nor cached.
    if (epoch !== this.#sessionEpoch) return null;
    // Same reasoning as refresh(): a truthy body is not a usable one, and
    // an unusable one must not reach the cache, where it would be served
    // to every reopen until the next poll.
    if (r.status === 200 && isFindingDetail(r.body)) {
      this.findingDetailCache.set(id, { revision: requestedAt, detail: r.body });
      return r.body;
    }
    this.findingDetailCache.delete(id);
    return null;
  }

  async fetchRepoNotes(id: number): Promise<string> {
    if (this.repoNotesCache.has(id)) return this.repoNotesCache.get(id)!;
    const epoch = this.#sessionEpoch;
    const r = await get<RepoNotesResponse>(`/api/repo/notes?id=${id}`);
    // Read under a session that has since ended: the cache was cleared
    // with it, and must not be refilled from the old one.
    if (epoch !== this.#sessionEpoch) {
      throw new Error(`GET /api/repo/notes?id=${id}: the session ended meanwhile`);
    }
    // An error body carries no `notes`; coercing that to "" and caching it
    // would show "No notes yet" forever with no retry, so fail instead and
    // let the caller's failure path handle it.
    if (!r.ok || typeof r.body?.notes !== "string") {
      throw new Error(`GET /api/repo/notes?id=${id} failed (status ${r.status})`);
    }
    this.repoNotesCache.set(id, r.body.notes);
    return r.body.notes;
  }
}

export const store = new HunterStore();
