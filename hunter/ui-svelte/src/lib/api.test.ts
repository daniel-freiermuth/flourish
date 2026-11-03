import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { GET_TIMEOUT_MS, POLL_MS, post, store } from "./api.svelte";
import { loadStatsHorizon, saveStatsHorizon } from "./statsHorizon";

/**
 * `store` is a module singleton; clear what the previous test wrote so an
 * assertion can only pass on data the current test landed — and so a test
 * that ended signed out does not leave every later refresh refused.
 */
function resetStore() {
  store.summary = null;
  store.findings = [];
  store.jobs = [];
  store.events = [];
  store.stats = null;
  store.error = null;
  store.user = null;
  store.needsLogin = false;
  store.statsHorizon = "all";
}

// Bodies shaped like what the daemon serves, trimmed to what the
// validators in validate.ts require — a refresh that fails validation
// writes nothing, which would make every assertion below vacuous.
let servedCounts: Record<string, number>;

// Stats answer with a job count that names the window they were asked
// for, so an assertion can tell one horizon's numbers from another's.
const JOBS_IN = { "24h": 1, "7d": 7, "30d": 30, "90d": 90, all: 1000 } as const;

function bodyFor(url: string): unknown {
  const [path, query] = url.split("?");
  switch (path) {
    case "/api/summary":
      return {
        backend_status_html: "<div></div>",
        counts: servedCounts,
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
    case "/api/stats": {
      const horizon = new URLSearchParams(query).get("horizon") as keyof typeof JOBS_IN;
      return { totals: { jobs: JOBS_IN[horizon] }, by_kind: [], by_finding: [] };
    }
    default:
      return [];
  }
}

/**
 * A network that answers nothing until told to. Every request is parked
 * in FIFO order; `release(n)` completes the n oldest, which is how a
 * refresh is held open across a poll tick.
 */
interface FakeNet {
  paths: string[];
  release(count: number): Promise<void>;
  /** Complete the `count` NEWEST requests, leaving older ones parked. */
  releaseNewest(count: number): Promise<void>;
}

function installNet(): FakeNet {
  const paths: string[] = [];
  const parked: (() => void)[] = [];
  vi.stubGlobal("fetch", (input: string, init?: RequestInit) => {
    paths.push(input);
    // Captured at request time, like a real server read: a response that
    // set out before a write cannot carry that write's result.
    const body = bodyFor(input);
    // Executor form, not Promise.withResolvers: tsconfig.app.json targets
    // es2023, which does not have it.
    return new Promise<Response>((resolve, reject) => {
      // A real fetch rejects with the signal's reason as soon as it
      // aborts. Without honouring that, a parked request would outlive
      // its deadline and the store's timeout would look like a no-op.
      const signal = init?.signal;
      if (signal) signal.addEventListener("abort", () => reject(signal.reason));
      parked.push(() => resolve({
        status: 200,
        ok: true,
        json: () => Promise.resolve(body),
      } as unknown as Response));
    });
  });
  // Drain the await chain inside refresh(): fetch, then json(), then
  // Promise.all, then the state writes. Microtasks only — none of it
  // depends on the faked clock.
  const settle = async () => {
    for (let i = 0; i < 20; i++) await Promise.resolve();
  };
  return {
    paths,
    async release(count: number) {
      for (const answer of parked.splice(0, count)) answer();
      await settle();
    },
    async releaseNewest(count: number) {
      for (const answer of parked.splice(-count)) answer();
      await settle();
    },
  };
}

// One refresh fans out to /api/summary, /api/findings, /api/jobs,
// /api/events and /api/stats.
const BATCH = 5;

describe("poll scheduling", () => {
  let net: FakeNet;

  beforeEach(() => {
    resetStore();
    servedCounts = { new: 1 };
    net = installNet();
    vi.useFakeTimers();
  });

  afterEach(async () => {
    store.stopPolling();
    // Let any refresh still parked finish, so the next test does not
    // start with a poll the store believes is in flight.
    await net.release(Number.POSITIVE_INFINITY);
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("issues no new requests while a poll refresh is still in flight", async () => {
    store.startPolling();
    expect(net.paths).toHaveLength(BATCH);

    await vi.advanceTimersByTimeAsync(POLL_MS * 3);

    expect(net.paths).toHaveLength(BATCH);
  });

  it("lands a refresh slower than the interval, then resumes polling", async () => {
    store.startPolling();
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);

    await net.release(BATCH);

    // The ticks that fired meanwhile must not have superseded this
    // result: a discarded refresh leaves the dashboard frozen with
    // `error` null, so App.svelte shows no staleness banner either.
    expect(store.summary?.counts).toEqual({ new: 1 });
    expect(store.error).toBeNull();

    // And the gate must reopen once the slow poll settles, or polling
    // stops for good.
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(net.paths).toHaveLength(BATCH * 2);
  });

  it("gives up on a poll the daemon never answers, then polls again", async () => {
    store.startPolling();

    // Nothing is ever released: the daemon accepted the connections and
    // went quiet. The gate holds every later tick off, and while it does
    // the dashboard keeps showing whatever it last read as though it were
    // live -- ending that silence is the whole job of the deadline.
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(net.paths).toHaveLength(BATCH);
    expect(store.error).toBeNull();

    await vi.advanceTimersByTimeAsync(GET_TIMEOUT_MS - POLL_MS * 3);

    // The request rejected and refresh() recorded it, which is what
    // raises App.svelte's "data may be stale" banner.
    expect(store.error).not.toBeNull();
    expect(store.error).toMatch(/GET \/api\/\S+ timed out/);

    // And the gate reopened: the first tick after the failure issues a
    // fresh batch instead of being skipped for good.
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(net.paths).toHaveLength(BATCH * 2);
  });

  it("reports a read that stalls mid-body as the timeout it is", async () => {
    // Headers arrived, the body never follows — the likely shape of a
    // stall on the largest response, the ~3MB /api/findings. The deadline
    // covers the body read too, and what it reports has to point at the
    // dead connection rather than at a response nobody malformed.
    vi.stubGlobal("fetch", (_input: string, init?: RequestInit) => {
      const stalledBody = new Promise<never>((_resolve, reject) => {
        const signal = init?.signal;
        if (signal) signal.addEventListener("abort", () => reject(signal.reason));
      });
      return Promise.resolve({
        status: 200,
        ok: true,
        json: () => stalledBody,
      } as unknown as Response);
    });

    const refresh = store.refresh();
    await vi.advanceTimersByTimeAsync(GET_TIMEOUT_MS);
    await refresh;

    expect(store.error).toMatch(/GET \/api\/\S+ timed out/);
  });

  it("runs a mutation refresh immediately and lets it win", async () => {
    store.startPolling();
    expect(net.paths).toHaveLength(BATCH);

    // A user action wrote; the poll already in flight left before that
    // write and will answer with pre-write data.
    servedCounts = { new: 42 };
    const mutation = store.refresh();
    expect(net.paths).toHaveLength(BATCH * 2);

    // The mutation answers FIRST, then the older poll. Releasing the
    // poll first would let the mutation's response land last and write
    // 42 by arrival order alone, so the assertion would hold with or
    // without the generation guard -- the exact thing under test.
    await net.releaseNewest(BATCH);
    await mutation;
    expect(store.summary?.counts).toEqual({ new: 42 });

    // Now the stale poll, carrying pre-write data. It must not win: a
    // response that set out before the write cannot be allowed to
    // overwrite the write's result.
    await net.release(BATCH);
    expect(store.summary?.counts).toEqual({ new: 42 });
  });

  // A switch of horizon races the poll sent with the old one. Whichever
  // order the two answers arrive in, the page must never show one
  // window's numbers under another window's caption.
  it("keeps a poll sent with the old horizon off screen when it lands first", async () => {
    store.startPolling();
    await net.release(BATCH);
    expect(store.stats?.totals.jobs).toBe(JOBS_IN.all);

    // The next poll leaves, still asking for `all`.
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(net.paths.slice(BATCH)).toContain("/api/stats?horizon=all");

    // The switch drops the old window's numbers at once and asks for the
    // new window without waiting for that poll to come back.
    store.setStatsHorizon("7d");
    expect(store.stats).toBeNull();
    expect(net.paths.slice(BATCH * 2)).toContain("/api/stats?horizon=7d");

    await net.release(BATCH);
    expect(store.stats).toBeNull();

    await net.release(BATCH);
    expect(store.stats?.totals.jobs).toBe(JOBS_IN["7d"]);
  });

  it("keeps a poll sent with the old horizon from overwriting the new one", async () => {
    store.startPolling();
    store.setStatsHorizon("7d");

    await net.releaseNewest(BATCH);
    expect(store.stats?.totals.jobs).toBe(JOBS_IN["7d"]);

    await net.release(BATCH);
    expect(store.stats?.totals.jobs).toBe(JOBS_IN["7d"]);

    // And every later poll asks for the new window.
    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(net.paths.slice(BATCH * 2)).toContain("/api/stats?horizon=7d");
    await net.release(BATCH);
    expect(store.stats?.totals.jobs).toBe(JOBS_IN["7d"]);
  });
});

// ---------------------------------------------------------------------------
// Stats horizon preference
// ---------------------------------------------------------------------------

describe("stats horizon preference", () => {
  let stored: Map<string, string>;

  beforeEach(() => {
    resetStore();
    stored = new Map();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => stored.get(key) ?? null,
      setItem: (key: string, value: string) => void stored.set(key, value),
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("survives a reload", async () => {
    const net = installNet();
    store.setStatsHorizon("30d");
    await net.release(Number.POSITIVE_INFINITY);

    // A reload evaluates the store module afresh, which a static import
    // cannot do: this is the module-loading boundary under test.
    vi.resetModules();
    const { store: reloaded } = await import("./api.svelte");
    expect(reloaded.statsHorizon).toBe("30d");
  });

  it("falls back to all on a value it does not know", () => {
    saveStatsHorizon("90d");
    expect(loadStatsHorizon()).toBe("90d");
    // Corrupt whatever was written, without pinning the storage key.
    for (const key of stored.keys()) stored.set(key, "1y");
    expect(loadStatsHorizon()).toBe("all");
  });

  it("falls back to all without usable storage, and saving does not throw", () => {
    vi.stubGlobal("localStorage", {
      getItem: () => { throw new Error("SecurityError"); },
      setItem: () => { throw new Error("QuotaExceededError"); },
    });
    expect(loadStatsHorizon()).toBe("all");
    expect(() => saveStatsHorizon("7d")).not.toThrow();
  });
});

// ---------------------------------------------------------------------------
// Session handling
// ---------------------------------------------------------------------------

/**
 * A daemon with the session rules of hunter-rs: every route but
 * /api/login answers 401 without a live session. Answers immediately,
 * except for paths listed in `held`, which park until released.
 */
interface FakeDaemon {
  /** Request log, `METHOD path`. */
  calls: string[];
  /** Bodies of the POSTs, by path. */
  posted: Map<string, unknown>;
  session: boolean;
  /** Who GET /api/me says the session belongs to. */
  meUser: string;
  held: Set<string>;
  releaseHeld(): Promise<void>;
}

const settle = async () => {
  for (let i = 0; i < 20; i++) await Promise.resolve();
};

function installDaemon(): FakeDaemon {
  const parked: (() => void)[] = [];
  const daemon: FakeDaemon = {
    calls: [],
    posted: new Map(),
    session: false,
    meUser: "alice",
    held: new Set(),
    async releaseHeld() {
      for (const answer of parked.splice(0)) answer();
      await settle();
    },
  };
  const route = (path: string, init?: RequestInit): [number, unknown] => {
    if (path === "/api/login") {
      const creds = JSON.parse(String(init?.body)) as { username: string; password: string };
      if (creds.password !== "hunter2") return [401, { error: "invalid username or password" }];
      daemon.session = true;
      return [200, { username: creds.username }];
    }
    if (!daemon.session) return [401, { error: "login required" }];
    if (path === "/api/logout") {
      daemon.session = false;
      return [200, { ok: true }];
    }
    if (path === "/api/me") return [200, { username: daemon.meUser }];
    if (path.startsWith("/api/repo/notes")) return [200, { notes: "old notes" }];
    if (path.startsWith("/api/finding?")) return [200, { jobs: [], pr_state: null }];
    return [200, bodyFor(path)];
  };
  vi.stubGlobal("fetch", (path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    daemon.calls.push(`${method} ${path}`);
    if (method === "POST") daemon.posted.set(path, JSON.parse(String(init?.body)));
    // Routed when sent, as the cookie is: a held request answers for the
    // session it carried, whatever happened while it was parked.
    const [status, body] = route(path, init);
    const response = {
      status,
      ok: status >= 200 && status < 300,
      json: () => Promise.resolve(body),
    } as unknown as Response;
    if (!daemon.held.has(path)) return Promise.resolve(response);
    return new Promise<Response>((resolve, reject) => {
      // As a real fetch does: an aborted request rejects with the reason.
      const signal = init?.signal;
      if (signal) signal.addEventListener("abort", () => reject(signal.reason));
      parked.push(() => resolve(response));
    });
  });
  return daemon;
}

describe("session", () => {
  let daemon: FakeDaemon;

  beforeEach(() => {
    resetStore();
    servedCounts = { new: 1 };
    daemon = installDaemon();
    vi.useFakeTimers();
  });

  afterEach(async () => {
    store.stopPolling();
    await daemon.releaseHeld();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  /** Dashboard reads issued so far, leaving out the session check. */
  const polls = () => daemon.calls.filter((c) => c.startsWith("GET /api/") && c !== "GET /api/me");

  it("shows the login form on a startup 401 and polls nothing", async () => {
    await store.start();

    expect(store.needsLogin).toBe(true);
    expect(store.user).toBeNull();
    expect(store.error).toBeNull();

    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(daemon.calls).toEqual(["GET /api/me"]);
  });

  it("polls straight away when the startup check finds a session", async () => {
    daemon.session = true;
    await store.start();
    await settle();

    expect(store.needsLogin).toBe(false);
    expect(store.user).toBe("alice");
    expect(store.summary?.counts).toEqual({ new: 1 });
  });

  it("switches to the dashboard after a successful login and starts polling", async () => {
    await store.start();

    expect(await store.login("alice", "hunter2")).toBeNull();
    expect(daemon.posted.get("/api/login")).toEqual({ username: "alice", password: "hunter2" });
    expect(store.needsLogin).toBe(false);
    expect(store.user).toBe("alice");

    // Refreshed at once, not one poll period later.
    await settle();
    expect(store.summary?.counts).toEqual({ new: 1 });
    expect(polls()).toHaveLength(BATCH);

    await vi.advanceTimersByTimeAsync(POLL_MS);
    expect(polls()).toHaveLength(BATCH * 2);
  });

  it("reports the server's message for a failed login and stays signed out", async () => {
    await store.start();

    expect(await store.login("alice", "wrong")).toBe("invalid username or password");
    expect(store.needsLogin).toBe(true);

    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(polls()).toHaveLength(0);
  });

  it("returns to the login form when a poll is refused, and stops polling", async () => {
    daemon.session = true;
    await store.start();
    await settle();
    expect(store.summary).not.toBeNull();

    // Session expired server-side between two ticks.
    daemon.session = false;
    await vi.advanceTimersByTimeAsync(POLL_MS);

    expect(store.needsLogin).toBe(true);
    expect(store.user).toBeNull();
    // The refusal is not an outage: no "may be stale" banner, and nothing
    // from the lost session left behind the form.
    expect(store.error).toBeNull();
    expect(store.summary).toBeNull();

    const after = daemon.calls.length;
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(daemon.calls).toHaveLength(after);
  });

  it("returns to the login form when a write is refused", async () => {
    daemon.session = true;
    await store.start();
    await settle();

    daemon.session = false;
    const r = await post("/api/verdict", { id: 1, status: "queued" });

    expect(r.status).toBe(401);
    expect(store.needsLogin).toBe(true);
  });

  it("logs out on the server, clears the dashboard and stops polling", async () => {
    daemon.session = true;
    await store.start();
    await settle();
    expect(store.summary).not.toBeNull();

    expect(await store.logout()).toBeNull();

    expect(daemon.calls).toContain("POST /api/logout");
    expect(daemon.session).toBe(false);
    expect(store.needsLogin).toBe(true);
    expect(store.user).toBeNull();
    expect(store.summary).toBeNull();
    expect(store.findings).toEqual([]);

    const after = daemon.calls.length;
    await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    expect(daemon.calls).toHaveLength(after);
  });

  it("ignores a 401 answering a request sent before the current login", async () => {
    await store.start();
    // Sent while signed out and still in flight when the user signs in:
    // its 401 speaks for the old state, not for the new session.
    daemon.held.add("/api/finding?id=1");
    const stale = store.fetchFindingDetail(1);

    expect(await store.login("alice", "hunter2")).toBeNull();
    await daemon.releaseHeld();
    await stale;

    expect(store.needsLogin).toBe(false);
    expect(store.user).toBe("alice");
  });

  it("ignores a startup session check answered after a new login", async () => {
    daemon.session = true;
    daemon.held.add("/api/me");
    const starting = store.start();

    // Someone else signs in on this page while the check is in flight.
    expect(await store.login("bob", "hunter2")).toBeNull();
    daemon.held.clear();
    await daemon.releaseHeld();
    await starting;

    expect(store.user).toBe("bob");
  });

  it("gives up on a login the server never answers, so the form can retry", async () => {
    await store.start();
    daemon.held.add("/api/login");
    const attempt = store.login("alice", "hunter2");

    await vi.advanceTimersByTimeAsync(GET_TIMEOUT_MS);

    expect(await attempt).toMatch(/timed out/);
    expect(store.needsLogin).toBe(true);
  });

  it("gives up on a logout the server never answers, so it can be retried", async () => {
    daemon.session = true;
    await store.start();
    await settle();
    daemon.held.add("/api/logout");
    const attempt = store.logout();

    await vi.advanceTimersByTimeAsync(GET_TIMEOUT_MS);

    expect(await attempt).toMatch(/timed out/);
  });

  it("does not refill the notes cache from a request of an ended session", async () => {
    daemon.session = true;
    await store.start();
    await settle();
    daemon.held.add("/api/repo/notes?id=1");
    const stale = store.fetchRepoNotes(1).catch((err: unknown) => err);

    expect(await store.logout()).toBeNull();
    daemon.held.clear();
    await daemon.releaseHeld();
    expect(await stale).toBeInstanceOf(Error);

    expect(await store.login("alice", "hunter2")).toBeNull();
    const before = daemon.calls.filter((c) => c.includes("/api/repo/notes")).length;
    await store.fetchRepoNotes(1);
    expect(daemon.calls.filter((c) => c.includes("/api/repo/notes"))).toHaveLength(before + 1);
  });

  it("does not show a finding detail read under an ended session", async () => {
    daemon.session = true;
    await store.start();
    await settle();
    daemon.held.add("/api/finding?id=1");
    const stale = store.fetchFindingDetail(1);

    expect(await store.logout()).toBeNull();
    daemon.held.clear();
    await daemon.releaseHeld();

    expect(await stale).toBeNull();
  });
});
