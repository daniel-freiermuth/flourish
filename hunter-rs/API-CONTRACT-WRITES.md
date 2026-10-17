# Hunter write-path API contract (Python -> Rust port, round 2)

Extracted from the Python source at commit state of 2026-09-13. Every claim cites `file:line`.
Sources: `hunter/hunter/server.py` (handlers), `hunter/hunter/store.py` (SQL), `hunter/hunter/types.py` (status enums), `hunter/hunter/forge.py` (forge detection), `hunter/hunter/scheduler.py` (deferred clone), `hunter/schema.sql` (DDL), `hunter/ui-svelte/src/` (consumer — the vanilla `hunter/ui/src/app.ts` this document was first written against no longer exists), `hunter/tests/` (behavioral pins).

Shared conventions (error envelope `{"error": "<msg>"}`, `_send` headers, Row shapes, per-request `Store`) are in `hunter-rs/API-CONTRACT.md` sections 1-2 and are not repeated here except where write-specific.

---

## 0. Shared POST plumbing

### 0.1 Request gates: Host and Content-Type — run before any handler

**Host gate (DNS-rebinding guard).** By default both daemons bind `127.0.0.1` only, so this is not about the network: a page the operator visits can point its own hostname at 127.0.0.1 and then talk to this port same-origin, which CORS does not stop. The one thing that still differs is the `Host` header, which carries the attacker's name. A `Host` whose name is not `localhost`, `127.0.0.1` or `::1` is refused with `403 {"error": "forbidden: non-local origin"}` — same status and message in both daemons. Rust accepts further names from `serve.allowedHosts` (loopback always stays accepted; `["*"]` accepts any name) and binds `serve.host`, so an operator can serve a LAN deliberately; Python has neither option. Cited by symbol, not line: Python `Handler._check_post_preconditions` in `server.py`; Rust `require_allowed_host`, `host_name` and the `host_guard` layer in `server.rs`. They differ, deliberately or by Python's age, in four ways:

- **Scope.** Python checks `Host` on **POST only**. Rust applies it as a router layer to **every route**, reads and static files included: findings carry file paths, code excerpts and repo notes from private repositories, and leaking those is the same class of loss as an unauthorised write. So a GET with a foreign `Host` is served by Python and refused by Rust. Pinned: `a_foreign_host_header_cannot_read_findings` (`hunter-rs/tests/server_test.rs`).
- **Order.** Python checks Content-Type first, then `Host`; Rust's layer runs before any handler, so before the Content-Type gate. A foreign `Host` with a wrong Content-Type is therefore `415` from Python and `403` from Rust.
- **Absent `Host`.** Python refuses it (`""` is not a loopback name); Rust allows it. Only an HTTP/1.0 client can omit the header, and a rebinding attack runs in a browser, which always sends one.
- **Parsing.** Rust strips a port per RFC 7230 §5.4 — `name`, `name:port`, `[v6]`, `[v6]:port`, stripping only an all-digit port — so `[::1]:8377` is accepted, and names like `:8377` or `::2` are refused. Python takes `split(":")[0]`, which cuts an IPv6 literal at its first colon: it refuses `[::1]` and bare `::1` despite listing `::1`. Unreachable for Python, which binds IPv4 only; Rust binds an IPv6 `serve.host` such as `::`, where the bracketed forms are what clients send.

**Content-Type gate (CSRF guard).**

`do_POST` (server.py:419-456) first reads `Content-Type` (default `''`) and requires `ctype.startswith('application/json')` (server.py:409-410). Failure -> `415 {"error": "Content-Type must be application/json"}` (server.py:411). Notes:

- **Prefix match**: `application/json; charset=utf-8` passes.
- The gate applies to **every POST route including `/api/cycle`**, which never reads a body.
- **Every UI POST clears the gate**: the Svelte UI routes all of them through one `post()` helper that sets `Content-Type: application/json` and a JSON body (`ui-svelte/src/lib/api.svelte.ts:104-110`); the bare `api()` wrapper sets no headers (`api.svelte.ts:32-45`) and is never called directly by a read either — every GET goes through `get()` (`:76-87`), which adds the abort deadline. The body-less `/api/cycle` call is `post("/api/cycle", {})` (`ui-svelte/src/pages/StatusPage.svelte:12`), i.e. it sends `{}` and passes. (This supersedes the old vanilla-`app.ts` discrepancy where `runCycle` sent no header and got a 415.)

**Session gate (Rust only).** Every `/api` route except `POST /api/login` — reads and writes, `/api/me` and `/api/logout` included — requires a live session: a `hunter_session` cookie whose token's SHA-256 is in `sessions`, not past `expires_at`, for an account whose `disabled_at` is NULL. Otherwise `401 {"error": "login required"}`, before any handler runs (`server::require_session`, a `route_layer` on the API sub-router). Order: Host gate, then session gate, then the handler (Content-Type gate, body parse). Static files and unknown paths are not gated: the bundle carries no data and has to load to show the login form. Pinned: `every_api_route_needs_a_session` (`hunter-rs/tests/post_test.rs`). Python has no accounts.

The cookie is `HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000` (30 days, no renewal), without `Secure` because the default listener is plain HTTP on loopback. `SameSite=Strict` stops other sites from sending it, so together with the Content-Type gate no cross-site request reaches a handler.

### 0.2 Route dispatch and error taxonomy

Dispatch is exact-path string equality on `urlparse(self.path).path` (server.py:422-450); unknown path -> `404 {"error": "not found"}` (server.py:451). The whole dispatch runs inside:

```python
except (ValueError, json.JSONDecodeError) as exc:   # server.py:452-453
    self._error(400, str(exc))
except Exception:                                    # server.py:454-456
    log.exception(...); self._error(500, "internal error")
```

Body parsing `_body_json` (server.py:176-186): reads exactly `Content-Length` bytes (no chunked support; missing/0 CL == empty).

| Condition | Exception | Response |
|---|---|---|
| empty body (no bytes) | `ValueError("empty body")` (server.py:179-181) | `400 {"error": "empty body"}` |
| malformed JSON | `json.JSONDecodeError` | Python: `400 {"error": "Expecting value: line 1 column 1 (char 0)"}` (or whichever decode message). **Rust: `400 {"error": "invalid JSON body"}`** — same status, fixed message, since the decoder message is serde's and not Python's and pinning it would pin a dependency's prose |
| valid JSON, not an object (array/string/number) | `TypeError("body must be a JSON object")` (server.py:183-185) | Python: **`500 {"error": "internal error"}`** — TypeError is NOT caught by the 400 branch. **ACCEPTED DEVIATION, Rust: `400 {"error": "body must be a JSON object"}`** — a client sending an array is making a bad request, not triggering a server fault, and the 400 envelope is what every other body error here returns. Pinned in `body_parse_errors`, tests/post_test.rs |
| `store.soft_delete_repo` refusal (see §8) | `ValueError` from store.py:651-656 | `400` with the store's message |
| non-string truthy `reason` on `/api/verdict` | `AttributeError` (`.strip()` on a non-str, server.py:462 — no `isinstance` guard there) | `500 {"error": "internal error"}` |

A non-string `name` on `/api/repos` used to belong in that row and no longer does: `_add_repo` guards with `raw.strip() if isinstance(raw, str) else ""` (server.py:633-634), so `{"name": 123}` becomes the empty string and is refused by the name rule. Both daemons answer `400 {"error": "invalid repo name"}` — Python because the empty string fails the regex (server.py:636-638), Rust because `as_str` on a non-string yields `""` before `valid_repo_name` ever sees it (`add_repo`, server.rs:1016-1023; `valid_repo_name` itself at server.rs:996-1003). Observed on the Python side: `{"name": 123}`, `{"name": {"a": 1}}` and `{"name": null}` each return exactly `b'{"error": "invalid repo name"}'` with status 400. The two daemons agree on status and on the JSON document, but **not byte-for-byte**: `json.dumps` writes `{"error": "invalid repo name"}` with a space after the colon and serde_json writes the compact `{"error":"invalid repo name"}` (observed live: the Rust daemon answers an unknown route with `{"error":"not found"}`). Compare parsed JSON in tests, never raw bytes.

**Python quirk to decide on**: `isinstance(True, int)` is `True`, so JSON `true`/`false` passes every `isinstance(fid, int)` check and is treated as id 1/0. The Rust port should accept only actual integers; no client sends booleans — every UI id comes from `finding.id` / `repo.id`, typed `number`.

### 0.3 Transaction model

- One fresh `Store` per request (`_store`, server.py:157-160) — a new SQLite connection that **re-runs `schema.sql` via `executescript` plus 28 `ALTER TABLE` migration probes on every construction** (store.py:166-284). Python's sqlite3 default busy timeout is 5 s `[INFERENCE from stdlib default]`; WAL mode (schema.sql:2).
- **Every store method commits itself** (e.g. store.py:603, 618, 669, 937, 981, 1061, 1366). No transaction spans two store calls: a handler doing `set_status` + `log_event` performs **two independent commits**; a crash between them loses the event but keeps the status change. The Rust port may keep this (parity) — do not wrap handler bodies in one transaction, it would change observable failure behavior only, which is acceptable, but sqlite row visibility mid-handler is identical either way.
- `updated_at` on `findings` is set to `now_ms()` (epoch **milliseconds**, types.py:90-91) by every findings write (store.py:925, 979, 1059). `repos` has **no** `updated_at` column (schema.sql:5-26); repo updates touch nothing but the named columns.

### 0.4 Status vocabulary (types.py)

- `FINDING_STATUSES` = all of `Status` (types.py:66): `new, rechecking, queued, fixing, pr_open, merged, rejected, wontfix, note` (types.py:23-32). Rust (`FindingStatus::ALL`) also includes `closed` and `superseded`. `blocked` existed until migration 025 turned every blocked fix into a hold (WORK-MODEL.md §11).
- Rust `VERDICT_STATUSES`: `queued, rejected, wontfix, note` (`closed` and `superseded` are scheduler-only; `merged` is set only by the PR sync from the forge's state — a deliberate break from Python, which accepted it as a verdict).
- Rust `REASON_REQUIRED`: `rejected, wontfix`.
- **Held work is not a status.** A hold stops one kind of a finding's work for the operator and leaves its status alone (WORK-MODEL.md §9): a held fix stays `queued`, its tier skips it. `/api/findings` shows each finding's active holds (`holds`, API-CONTRACT.md §4) and `POST /api/hold/release` (§11 below) ends one. Budget denial leaves a hold and its checkpoint untouched.
- **Who owns a finding.** A finding at `new`, `note` or `closed` waits for the operator, and so does held work at any status; every other status belongs to a job (`queued`/`fixing`: fix, `rechecking`: recheck, `pr_open`: engage) or the forge. Operator writes only start from the statuses that ask for them (`/api/verdict`: `new`/`note`/`closed`, `/api/hold/release`: an active hold, `/api/recheck`: `new`, `/api/unqueue`: `queued`), and each is a single UPDATE conditional on the status it just checked, so a job that claims the finding in between keeps it (409). `run_fix` claims `queued` → `fixing` the same way before creating its job; if the operator unqueued the finding after it was picked, the claim fails, the cycle is `skipped`, and the daemon starts the next cycle at once. So nothing changes a finding's status while its job runs, and the jobs' completion writes need no guard of their own.

Transition legality as enforced by the server:

| Endpoint | Precondition on current status | New status | Reason |
|---|---|---|---|
| `/api/verdict` | Rust: `new`, `note` or `closed`, and different from the new status (Python: none — any current status, including `fixing`/`pr_open`) | any of `VERDICT_STATUSES` | mandatory iff new status in `rejected, wontfix`; optional otherwise |
| `/api/recheck` | exactly `new` (Rust: conditional write, 409 if it changed after the check) | `rechecking` | n/a |
| `/api/unqueue` | exactly `queued` (Rust: conditional write, 409 if it changed after the check) | `new` | n/a |

`store.set_status` additionally validates membership in `FINDING_STATUSES` and raises `ValueError(f"invalid status: {status}")` (store.py:921-923) — unreachable from these handlers since `VERDICT_STATUSES ⊂ FINDING_STATUSES`.

---

## 1. POST /api/verdict — human triage verdict

Handler `_verdict` (server.py:458-487).

**Request**: `{"id": int, "status": str, "reason"?: str}`. `reason` is normalized `(body.get("reason") or "").strip() or None` (server.py:462) — absent, null, empty, or whitespace-only all become None.

**Validation order** (first failure wins):
1. `id` not an int -> `400 {"error": "id must be an integer"}` (server.py:463-465).
2. `status not in VERDICT_STATUSES` -> `400` with `f"status must be one of {list(VERDICT_STATUSES)}"` (server.py:466-471). Because `Status` is a `StrEnum`, the f-string renders **enum reprs**, byte-exact: `status must be one of [<Status.QUEUED: 'queued'>, <Status.REJECTED: 'rejected'>, <Status.WONTFIX: 'wontfix'>, <Status.NOTE: 'note'>, <Status.MERGED: 'merged'>]` `[INFERENCE from CPython StrEnum repr; UI only displays the string]`. String comparison works because StrEnum members `==` their values. Rust emits the plain values and, without `merged`, refuses it here: `status must be one of ['queued', 'rejected', 'wontfix', 'note']`.
3. `status in REASON_REQUIRED and not reason` -> `400 {"error": "reason required for status 'rejected'"}` (repr-quoted status, server.py:472-474).
4. `get_finding(fid)` is None -> `404 {"error": "no finding <fid>"}` (server.py:476-479). `get_finding` = `SELECT * FROM findings WHERE id = ?` (store.py:859-861).
5. **Rust only (deliberate break from Python):** the current status is not one that asks the operator for a verdict (`new`, `note`, `closed`; `FindingStatus::awaits_verdict`), or equals the new status -> `400 {"error": "finding #<id> is '<status>'; a verdict applies only to new, note or closed findings, and must change its status"}`. `closed` is included because a closed-PR harvest that gives up leaves the finding `closed` and never revisits it, so only a verdict can settle it; a harvest still running keeps the verdict (`record_closed_harvest` only replaces `closed`). Everywhere else a job (`fixing`, `rechecking`, `pr_open`) or the forge (`merged`) owns the finding. The write is conditional on the status just read, so a job that claims the finding in between keeps it: `409 {"error": "finding #<id> changed while the verdict was being applied; reload and retry"}`.

**Writes** (server.py:480-486):
1. `set_status(fid, status, verdict_reason=reason)` -> `UPDATE findings SET status = ?, updated_at = ?[, verdict_reason = ?] WHERE id = ?` + commit (store.py:913-937). `verdict_reason` is only included when reason is non-None — **a verdict without reason does NOT clear a previously stored verdict_reason**. **Rust (deliberate break):** the verdict is one `Transition::Verdict` write (`Store::transition`), conditional on the status read in step 5, that sets `verdict_reason` to the reason or NULL and removes any verdict anchor: a verdict replaces the earlier one whole, so a reason-less `closed → queued` no longer keeps "PR closed without merge; awaiting harvest".
2. `log_event("verdict", f"finding {fid} [{finding['fingerprint']}] -> {status}" + (f": {reason}" if reason else ""), finding_id=fid)` -> `INSERT INTO events (at, kind, message, job_id, finding_id) VALUES (?,?,?,?,?)` + commit (store.py:1355-1366). `fingerprint` is from the row read **before** the update.

**Success**: `200 {"ok": true, "finding": <full refreshed row>}` — re-fetched via `get_finding(fid)` (server.py:487), i.e. `SELECT *` on findings: all columns incl. migration-added ones (see round-1 contract's Finding row shape).

**UI**: two callers, both sending `{id, status}` plus `reason` only when non-empty. `FindingCard.doVerdict` (`components/FindingCard.svelte:46-64`) passes `reason: reason || undefined`, which `JSON.stringify` drops; `KanbanPage.verdict` (`pages/KanbanPage.svelte:55-71`) spreads `...(reason ? {reason} : {})`. Neither reads the embedded finding — success is `await store.refresh()`. Failure is `!r.ok` (never a status comparison): `console.error` with the status, and **no user-visible message on this endpoint** — the card simply stays as it was, and in `FindingCard` the reason prompt and its text are deliberately kept so the verdict can be retried (:50-53). Reason for `rejected`/`wontfix` is required client-side in both: `FindingCard` renders an inline input whose Confirm button is `disabled={busy || !reasonText.trim()}` (:192-211), `KanbanPage` uses `prompt()` and calls `verdict` only `if (reason)` (:165-168). `KanbanPage` additionally guards concurrency with a `pending` set keyed by finding id, since the two verdict buttons write the same row (:53-57). Python's UPDATE is unconditional; in Rust each write is conditional on the status its handler just read, so two racing verdicts cannot both apply: the second gets step 5's `409`.

---

## 2. POST /api/cycle — manual cycle trigger

Handler `_cycle` (server.py:489-518). **Never reads the body** (only the §0.1 gate applies).

**Flow**:
1. `_cycle_lock.acquire(blocking=False)` — module-global `threading.Lock` (server.py:93). Held -> `409 {"error": "busy"}` via `_json` (server.py:490-492).
2. Else spawn `threading.Thread(target=run, name="hunter-cycle", daemon=True)` (server.py:517) and respond **immediately** `202 {"started": true}` (server.py:518).
3. The thread (server.py:496-515): fresh `Store(cfg)`; `_reconcile_and_log(store)` (server.py:118-139); `scheduler.run_cycle(store, cfg, backend=self.backend)`; on any exception logs + best-effort `log_event("error", "cycle failed (see logs)")` (server.py:510-513); `finally: _cycle_lock.release()` (server.py:514-515).

**State it mutates / needs from in-process state**:
- `_cycle_lock` — shared with the daemon loop (`daemon()` acquires it non-blocking each wake, server.py:1266, releases 1299) and read by `GET /api/summary` as `cycle_running = _cycle_lock.locked()` (server.py:311). This is what makes manual and timed cycles mutually exclusive and what drives the `"working"` activity status (server.py:1128-1129).
- `Handler.backend` — the singleton `Backend` built once per process in `serve()`/`daemon()` from `cfg.make_backend(ThreadLocalLedger(cfg))` (server.py:923, 1245; types.py:232-243).
- Reconcile writes (store.py:1303-1352): findings stuck at `'fixing'` -> `set_status(id, "queued")` (store.py:1339-1341); jobs stuck at `'running'` -> `update_job(state="killed", killed_reason="orphaned", finished_at=now_ms(), notes="reconciled at cycle startup -- prior process died mid-job")` (store.py:1343-1351; `update_job` store.py:1191-1198). Each touched row also gets a `log_event("error", ...)` with the exact formats at server.py:126-139 (`f"reconciled #{f['id']} stuck 'fixing' -> 'queued' -- prior process died mid-fix"` / `f"reconciled orphaned {r['kind']} job #{r['id']} (finding {r.get('finding_id')}) -- prior process died mid-job"`).
- `run_cycle` itself spawns workers, writes jobs/findings/pr_state, shells out to git/gh — the entire scheduler.

**Rust implementation**: `AppState.scheduler: SchedulerHandle` carries the job lane's `running`, `paused`, and `overdrive` atomics plus the job lane's `wake` and the admin lane's PR sync duty's `pr_sync_wake` notifies. `/api/cycle` first notifies the PR sync, so open PRs are synced at once whatever the job lane or the admin lane's other duties are doing (a pause stops only jobs, §2.1). Then it returns 409 `{"error":"paused"}` while paused, 409 `{"error":"busy"}` while a cycle is running, or notifies the job lane and returns 202 `{"started":true}`. The busy check is advisory: a trigger racing the loop into its next cycle is held as a notify permit and runs on the following iteration instead of being dropped.

**UI** (`StatusPage.runCycle`, `ui-svelte/src/pages/StatusPage.svelte`): `post("/api/cycle", {})` — header and `{}` body per §0.1, so the gate passes and the real status codes are reached. This is the one caller that compares `status` to literals rather than reading `r.ok`, because here the code *is* the information: 409 (`busy` or `paused`; the PR sync was woken either way) -> button text "syncing PRs…", 202 -> "started…", anything else -> "error"; then `store.refresh()`. While a cycle runs or the jobs are paused, the button reads "Sync PRs now"; it is never disabled except while a request is in flight. The button is re-armed after 2.5 s in `finally`, so it recovers even when the request never reached the server.

---
## 2.1 POST /api/scheduler — pause or resume the scheduler

**Request**: `{"paused": boolean}`. Missing or non-boolean values return
`400 {"error":"paused must be a boolean"}`.

**Success**: `200 {"paused": <value>}`. The process-local flag changes
immediately and both lanes are notified. Pausing blocks new job-lane cycles;
a worker already running is allowed to finish. It does not stop the admin
lane, which spends no tokens: PR sync, the reaper, fetch-ahead, the dependency scans and the
workspace sweep keep running, so merges, closes and review comments are still recorded
while jobs are paused. The flag resets to false on restart.

## 2.2 POST /api/overdrive — global prioritized budget mode

**Request**: `{"enabled": boolean}`. Missing or non-boolean values return
`400 {"error":"enabled must be a boolean"}`.

**Success**: `200 {"overdrive": <value>}`. The process-local flag changes
immediately and the loop is notified, so a budget-denied scheduler can retry.
While enabled, `OverdriveBackend` promotes the provider backend's
`Outlook.prioritized` verdict to the normal path for every job. This bypasses
pacing ramps but does not synthesize a grant: provider exhaustion and zero
hard-limit headroom remain denied. The flag resets to false on restart.

The Status-page toggle refreshes `/api/summary` after the write and renders
from `scheduler_overdrive`; it can be changed while the scheduler is paused.

---


## 3. POST /api/recheck — queue a `new` finding for recheck

Handler `_recheck` (server.py:520-539).

**Request**: `{"id": int}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:523-525).
2. no finding -> `404 {"error": "no finding <fid>"}` (server.py:527-530).
3. `finding["status"] != "new"` -> `400` with `f"finding #{fid} is {finding['status']!r}, not 'new'"` — e.g. `finding #7 is 'queued', not 'new'` (server.py:531-536).

**Writes**: `set_status(fid, "rechecking")` (SQL as §1, no verdict_reason); `log_event("recheck", f"#{fid} queued for recheck", finding_id=fid)` (server.py:537-538).

**Success**: `200 {"queued": true, "finding": <refreshed get_finding(fid) row>}` (server.py:539). Note the key is `queued`, not `ok`.

**UI** (`FindingCard.doRecheck`, `components/FindingCard.svelte:66-80`): sends `{id}`; the button renders only when `finding.status === "new"` (:220-223), so the 400 for a non-`new` status is reachable only by racing the poll. `!r.ok` -> `console.error` only, no user-visible message; success -> `await store.refresh()`. The `busy` flag is cleared in `finally`, re-arming the button even when the request never reached the server (:77-79).

---

## 4. POST /api/unqueue — remove a `queued` finding from the fix queue

Handler `_unqueue` (server.py:541-560). Mirror of §3:

1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:544-546).
2. no finding -> `404 {"error": "no finding <fid>"}` (server.py:548-551).
3. status != `"queued"` -> `400` `f"finding #{fid} is {finding['status']!r}, not 'queued'"` (server.py:552-557).

**Writes**: `set_status(fid, "new")`; `log_event("unqueue", f"#{fid} removed from fix queue", finding_id=fid)` (server.py:558-559).

**Success**: `200 {"ok": true, "finding": <refreshed row>}` (server.py:560).

**UI** (`FindingCard.doUnqueue`, `components/FindingCard.svelte:82-96`): sends `{id}`; the button renders only when `finding.status === "queued"` (:225-228). `!r.ok` -> `console.error` only; success -> `await store.refresh()`.

---

## 5. POST /api/override — budget override set/clear (+ daemon wake)

Handler `_override` (server.py:562-592).

**Request**: `{"id": int | "all", "mode": "once" | "exempt" | null}` (`mode` absent == null, server.py:565).

**Validation order**:
1. **Special case first**: `id == "all" && mode == null` -> `clear_all_overrides()` -> `UPDATE findings SET budget_override = NULL, updated_at = ? WHERE budget_override IS NOT NULL` + commit, returns `rowcount` (store.py:1054-1062); `log_event("override", f"cleared all budget overrides ({n} findings)")` (server.py:569, no finding_id); respond `200 {"ok": true, "cleared": n}` (server.py:570). `"all"` with a non-null mode falls through to step 2.
2. non-int id -> `400 {"error": "id must be an integer (or 'all' with mode=null)"}` (server.py:572-574).
3. mode not in `("once", "exempt", None)` -> `400 {"error": "mode must be 'once', 'exempt', or null"}` (server.py:575-577).
4. no finding -> `404 {"error": "no finding <fid>"}` (server.py:579-582).

**Writes**: `set_budget_override(fid, mode)` -> `UPDATE findings SET budget_override = ?, updated_at = ? WHERE id = ?` + commit (store.py:972-981; store re-validates mode, raising ValueError -> 400, unreachable here); `log_event("override", f"#{fid} budget override: {label}", finding_id=fid)` where `label = mode or "cleared"` (server.py:583-589).

**Success**: `200 {"ok": true, "finding": <refreshed row>}` (server.py:590).

**Wake side effect** (server.py:591-592): **after** the response is written, `if mode: _wake.set()` — only when *setting* (`once`/`exempt`), never when clearing. `_wake` is a module-global `threading.Event` (server.py:95). The daemon loop's sleep is a poll loop `while not stop and not _wake: stop.wait(min(remaining, 5.0))` (server.py:1305-1310), so a set event is noticed within ≤5 s and the next cycle attempt starts immediately; `_wake` is cleared at the top of each loop iteration (server.py:1267, 1302). Pinned by `tests/test_override_wake.py` (real HTTP server; asserts wake for once/exempt, and that clear does NOT wake).

**Why the wake cannot be dropped**: without it the override still takes effect — `pick_next`/`backend.decide` read `budget_override` from the DB — but only at the loop's next *natural* wake. Per `_compute_sleep_s` (server.py:1141-1202) that can be: 60 s (repos enabled, no work), 15 min (idle/unrecognized), 5 min (error), 30 min (denied without retry_at), or **up to 60 min** (denied with retry_at, cap `min(until_retry+30, 60*60)`, floor 60 s). Worst case ≈ 60 minutes — exactly the delay the override exists to bypass. **In Rust this is local, not forwarded**: the scheduler loop runs in this process, so `override_` notifies `AppState.scheduler`'s `wake` directly, and only when *setting* a mode, matching Python's `if mode:` (server.rs:806-808). Under `serve` (no scheduler) the write still lands; there is simply no loop here to wake.

**UI** (`FindingCard.doOverride`, `components/FindingCard.svelte:98-112`): two call shapes, `{id, mode: "once"|"exempt"}` and `{id, mode: null}`, chosen by whether `finding.budget_override` is set (:231-236). `!r.ok` -> `console.error` only; success -> `await store.refresh()`. **Known gap**: the `{"id": "all", "mode": null}` clear-all shape of step 1 has *no* caller in the Svelte UI — the vanilla `app.ts` had a `clearAllOverrides` button and nothing replaced it. The endpoint behaviour is still specified above and still implemented by both daemons; it is simply unreachable from the shipped UI today.

---

## 6. POST /api/repo — update repo fields

Handler `_update_repo` (server.py:594-627).

**Request**: `{"id": int, "enabled"?: any, "url"?: str, "default_branch"?: str, "forge"?: "github"|"gitlab"}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:597-599).
2. no repo -> `404 {"error": "no repo <rid>"}` (server.py:601-604). `get_repo` (store.py:585-590) — int key -> `WHERE id = ?`.
3. Field extraction (server.py:605-620), silently skipping anything invalid:
   - `enabled` present (any JSON type) -> coerced `1 if truthy else 0`.
   - `url` only if a string that is non-empty after `.strip()` -> stripped value, which must then pass the repo URL rule below. This is the one field in this handler that *rejects* rather than skips: a bad scheme is a `400`, not a dropped field (server.rs:833-844).
   - `default_branch` same rule.
   - `forge` only if literally `"github"` or `"gitlab"`.
4. No field survived -> `400 {"error": "no valid fields to update"}` (server.py:621-623). (So `{"id":1,"forge":"bitbucket"}` yields this, not a forge-specific error.)

**Repo URL rule** — `valid_repo_url` (server.rs:959-991; server.py:728-757). Both daemons enforce the same rule, and the two implementations were compared clause by clause: Python's `url.partition(":")` / `re.fullmatch(r"[A-Za-z][A-Za-z0-9+.\-]*", head)` / `head.lower() in ("http", "https", "ssh")` (server.py:748-757) is the same decision as Rust's `find(':')` / scheme charset check / `["http", "https", "ssh"].iter().any(|s| scheme.eq_ignore_ascii_case(s))` (server.rs:969-990). They must not drift: the two servers share a work root and a database, so a URL one accepts and the other refuses turns a rollback into a behaviour change. The rule, in order, after a leading `-` is refused outright (git would read the value as an option — `git clone --upload-pack=<cmd>`; server.rs:966-968; server.py:746-747):
1. No `:` anywhere -> accepted (server.rs:969-971; server.py:748-750). A bare path is a legal clone source.
2. There is a `:`, but the text before it is not an RFC 3986 scheme — scheme = ALPHA followed by ALPHA/DIGIT/`+`/`-`/`.` — -> accepted (server.rs:972-981; server.py:751-752). This is what keeps scp-like clone URLs working: in `git@github.com:acme/widget.git` the prefix is `git@github.com`, and `@` is not scheme-legal, so the colon is path syntax, not a scheme.
3. There is a scheme -> it must be `http`, `https` or `ssh`, matched case-insensitively (server.rs:988-990; server.py:757). `https://host/x.git`, `HTTPS://host/x.git`, `ssh://git@host/x.git` and `SSH://git@host/x.git` are all accepted; `JavaScript:alert(1)` is not.
4. Any other scheme — `javascript:`, `data:`, `vbscript:` — -> `400 {"error": "url must be http(s) or an ssh clone URL"}` (`ApiError::BadRequest` -> `StatusCode::BAD_REQUEST`, server.rs:107; `self._error(400, ...)` server.py:610, :643).

**Both spellings of an ssh clone URL are accepted**: the scheme-less scp-like `git@host:owner/repo.git` (no scheme, so it passes the first test) and the explicit `ssh://git@host/owner/repo.git` (`ssh` is in the allow-list alongside `http`/`https`, case-insensitively in both daemons). The guard exists to keep a URL that a browser would *execute* out of an `href` — `javascript:`, `data:`, `vbscript:` — not to restrict transports.

This was drafted the other way round, when the allow-list was `http`/`https` only and the explicit `ssh://` form was refused by a message that claimed to permit it. The code was the wrong side of that: the allow-list now includes `ssh`.

The gate is on the write because `repo.url` is rendered as a link by the UI. Not because the UI would execute one: `ReposPage.svelte` builds an `<a href>` only when `isHttpUrl(repo.url)` accepts it and otherwise renders the value as plain text (ReposPage.svelte:3, :353; `isHttpUrl`, format.ts:132), so a stored `javascript:` URL is inert there today. The write gate is what makes that a second line rather than the only one — the UI is not the only consumer of this column, a guard on one page is one refactor away from being dropped, and validating once at the write is cheaper than escaping every read (server.rs:946-953, server.py:728-732). It guards both write paths in both daemons with the identical message — update (server.rs:836-840; server.py:609-611, before the value reaches `fields`) and add (server.rs:1034-1038; server.py:642-644, after the non-empty check) — and is pinned on both sides: tests/post_test.rs:951-983 (add: the three executable schemes plus mixed-case `JavaScript:` rejected; `https`, `http`, scp-like `git@github.com:acme/widget.git` and explicit `ssh://git@github.com/acme/widget.git` accepted) and :985-1004 (update path); `hunter/tests/test_server.py:547-579` walks the same table against `valid_repo_url`, including `SSH://git@host/x.git` accepted and `ftp://example.com/x.git` rejected — the allow-list is exactly those three schemes, not "anything that is not executable".

**Writes**: `update_repo(rid, **fields)` -> dynamic `UPDATE repos SET <k> = ?, ... WHERE id = ?` + commit; allowed columns `{name, url, default_branch, forge, enabled}` else ValueError (store.py:605-618 — `name` is store-allowed but the handler never sends it). No `updated_at` (column doesn't exist). Then `log_event("repo", f"updated {repo['name']}: {action}")` where `action = ", ".join(f"{k}={v}")` over post-coercion values, e.g. `updated myrepo: enabled=1` (server.py:625-626; no finding_id/job_id).

**Success**: `200 {"ok": true, "repo": <refreshed get_repo(rid) row>}` (server.py:627) — every `repos` column except `deleted_at`, which `_repo_row` strips on the way out (store.py:116-128, 585-590): the 10 schema columns plus the 6 migration-added `last_*_at` ones. Identical shape to a `/api/repos` element (API-CONTRACT.md §7); `/api/summary.repos` is the one that differs.

**UI** (`ReposPage.toggleRepo`, `pages/ReposPage.svelte:94-114`): only the pause/enable toggle, sending `{id, enabled}` where `enabled` is the **integer** `0`/`1`, not a boolean (:95) — step 3's coercion accepts either. Branches on `r.ok`: success -> `toast("<name> enabled|paused", true)` + `store.refresh()`; failure -> `console.error` with the status plus `toast("Failed to toggle <name>", false)`, and the row is left showing its old state (:101-113). `url`/`default_branch`/`forge` updates still have no UI caller.

---

## 7. POST /api/repos — add repo

Handler `_add_repo` (server.py:629-662); Rust `add_repo` (server.rs:1005-1108). Cite the function names first — the line numbers below are hints that rot on every edit.

**Request**: `{"name": str, "url": str, "branch"?: str, "forge"?: "github"|"gitlab"}`.

**Validation order**:
1. `raw = body.get("name")`; `name = raw.strip() if isinstance(raw, str) else ""` (server.py:633-634). A non-string name is **not** a crash — it becomes `""` and falls into step 2's `400 {"error": "invalid repo name"}`, the same answer Rust gives (§0.2).
2. Rust: invalid name -> `400 {"error": "invalid repo name"}` (`valid_repo_name`, server.rs). A name is valid when it is non-empty after the trim, at most 100 characters (`MAX_REPO_NAME_CHARS`, GitHub's own limit), and free of control characters (`char::is_control`: newlines, tabs, NUL, ...) a Unicode line/paragraph separator (U+2028, U+2029 — category Zl/Zp, which `is_control` misses) or a Unicode `Bidi_Control` character (U+061C, U+200E–200F, U+202A–202E, U+2066–2069), which reorders the surrounding text where the UI shows the name. Spaces, `/`, `..` and non-ASCII are all fine: the name is a display label that never reaches a path (step 8), and what the rule protects is the event lines, log lines and worker prompts the name is printed into. **Deliberate divergence from Python**, whose rule is still `re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_.\-]*', name)` (server.py:636-638), a leftover from when the clone path was `repos/<name>`. Pinned by `repo_add_bad_name_and_missing_url` (rejects) and `repo_add_name_never_reaches_the_path` (accepts `My Repo (fork)`, `../../etc`, `café` and a 100-character name, each cloned into `repo-<id>`).
3. `url`: taken only if `isinstance(str)`, stripped; empty -> `400 {"error": "name and url are required"}` (server.py:635, 639-641; server.rs:1025-1033). The stripped value then goes through the **repo URL rule** (§6, `valid_repo_url`) -> `400 {"error": "url must be http(s) or an ssh clone URL"}` on a scheme outside the allow-list (server.py:642-644; server.rs:959-991, applied at server.rs:1034-1038), before `branch`/`forge` are looked at.
4. `branch`: `"main"` unless a non-blank string (server.py:645-646; server.rs:1039-1042).
5. `forge = body.get("forge") or None`; a falsy `forge` (absent, null, `""`) -> `detect_forge(url)` (server.py:647-649; server.rs:1062, the `_` arm of the `forge` match). **Detection is total**: it always returns a forge, so only an *explicit* bad forge reaches step 6's 400.

   The rule is one function per daemon and they agree clause for clause: `detect_forge` is `is_github_host(url_host(url))` -> `github`, else `gitlab` (`detect_forge`, forge.rs:921-927; forge.py:625-649, where the host comes from `_extract_host`, the Python spelling of `url_host`).

   `url_host` / `_extract_host` (forge.rs:875-900; forge.py:601-622) parses exactly the URL forms §6 admits, and nothing else:
   - **`https://`, `http://`, `ssh://`** — the authority is everything up to the first `/` (forge.rs:879; forge.py:613-615).
   - **scp-like `git@host:path`** — the authority is everything before the first `:`; that colon is the path separator, not a port (forge.rs:886-888; forge.py:617-620). **The authority half may not contain a `/`**: `github.com/x:y` is not scp syntax, so both daemons refuse to read it as host `github.com/x` and return no host at all (the `!a.contains('/')` filter, forge.rs:889; the `[^/:]+` class, forge.py:617-619). §6 accepts that string as a scheme-less URL, so it is storable — it simply has no parsed host, which means `detect_forge` falls to its `gitlab` default. Divergence here is the point: reading it as a host split the two daemons, storing `forge=github` in one and `forge=gitlab` in the other for the same POST.
   - **Only the scheme is case-folded.** `strip_scheme` compares it with `eq_ignore_ascii_case` (forge.rs:304-311, dispatched by `strip_any_scheme` forge.rs:314-318); Python uses `(?i:https?|ssh)` (forge.py:613). Schemes are case-insensitive per RFC 3986 and §6 stores `HTTPS://…` verbatim, so a case-sensitive compare here left the scheme inside the authority and read `HTTPS` as the host. The host is returned with its case intact and folded only inside `is_github_host`; the path is never folded, because it carries the case-sensitive owner/repo slug (`_url_path`, forge.py:120-130; `github_owner_repo`, forge.rs:270-290). Pinned: `HTTPS://github.com/Acme/Widget.git` -> host `github.com`, forge `github`, slug `Acme/Widget` (`TestCaseInsensitiveScheme`, test_forge.py:414-483).
   - **`user@` is stripped** from the authority, and **`:port` is stripped** — for the scheme forms only, since in the scp form the colon left with the path (forge.rs:891-898; forge.py:621-622). So `ssh://git@gitlab.example.com:2222/g/r.git` has host `gitlab.example.com`.
   - **Anything else has no host**: `None` / `""` (forge.rs:889, :899; forge.py:617-619). There is no hostname fallback. That fallback was the literal `"gitlab.com"`, which filed every `ssh://` URL as GitLab and ran `glab --hostname github.com` against GitHub repos (forge.py:607-611).

   `is_github_host` (forge.rs:258-262; forge.py:133-146) lowercases the host, splits it on `.`, and matches **labels, never substrings**. A host is GitHub iff some label is exactly `github`, **or** its last two labels are `ghe` then `com`. Everything else is GitLab — including the no-host case, since `detect_forge` feeds the empty string straight in (forge.rs:922; forge.py:646-649). There is no `gitlab` matching of any kind: GitLab is purely the remainder. The asymmetry is deliberate (forge.rs:904-910; forge.py:626-645): GitHub is reachable only at domains GitHub operates — `github.com` and Enterprise Cloud's `<org>.ghe.com` — plus Enterprise Server installs, which by convention carry a `github` label; any hostname at all can be a self-hosted GitLab, so GitLab takes the remainder. The label rule is a sanity bound, not an anti-spoofing measure: `github.com.evil.example` has a `github` label and reads as GitHub, and nothing here defends against a hostile URL — the URL is typed by the operator adding their own repo.

   | URL | host | detected | why |
   |---|---|---|---|
   | `https://gitlab.mycompany.com/g/r.git` | `gitlab.mycompany.com` | `gitlab` | no `github` label, not `*.ghe.com`. The `gitlab` label is matched by nothing; it lands in the remainder like any other host |
   | `https://code.mycompany.com/g/r.git` | `code.mycompany.com` | `gitlab` | names neither forge -> remainder. **Not `github`** |
   | `https://mycompany.ghe.com/acme/widget.git` | `mycompany.ghe.com` | `github` | last two labels are `ghe` + `com`; so is `git@mycompany.ghe.com:acme/widget.git` |
   | `https://notgithub.com/acme/widget.git` | `notgithub.com` | `gitlab` | the label is `notgithub`, not `github`. **A substring test would have said GitHub** |
   | `bogus` | (none) | `gitlab` | unparseable -> no host -> remainder |

   Pinned on both sides: `forge_is_detected_from_the_host` (hunter-rs/tests/forge_test.rs:247-302) carries all five rows; `TestDetectForge` (test_forge.py:314-344) and `TestExtractHost` (test_forge.py:290-311) are the Python mirror.
6. `forge not in FORGE_NAMES` (`("github", "gitlab")`, forge.py:594-598) -> `400 {"error": "unknown forge 'bitbucket' (choose from github, gitlab)"}` (repr-quoted, server.py:650-652; Rust parses `ForgeName` and formats the same message, server.rs:1043-1061).
7. Duplicate: `get_repo(name) is not None` -> `409 {"error": "repo 'x' already exists"}` (repr-quoted, server.py:653-656). **Gotcha**: `get_repo` routes all-digit keys to `WHERE id = ?` (store.py:585-590), so a repo *named* `"123"` dupe-checks against repo **id** 123 — a preexisting oddity the name rule permits; Rust ports it as-is (server.rs:1064-1074).
8. **No containment check** — there is nothing left to contain. The clone directory is `<work_root>/repos/repo-<id>`, derived from the id SQLite issues, so the handler passes `cfg.work_root / "repos"` to `add_repo` and never joins the name to a path at all (server.py:657-659; server.rs:1078-1089). Traversal is structurally impossible rather than filtered: a name of `../../etc` is accepted by Rust and addresses no path. Earlier revisions resolved `work_root / 'repos' / name` and required the result to be a prefix match under `work_root`; that check went with its premise.

**Writes**: `add_repo(name, url, cfg.work_root / "repos", branch, forge=forge)` (server.py:659) -> one transaction, two statements, because the path needs the id the INSERT has not issued yet: `INSERT INTO repos (name, url, path, forge, default_branch, added_at) VALUES (?,?,'',?,?,?)` with `added_at = now_ms()`, then `UPDATE repos SET path = ? WHERE id = ?` with `repo_dir(repos_dir, rid)` = `<repos_dir>/repo-<id>`; commit; returns `lastrowid` (`Store.add_repo` store.py:552-583, `repo_dir` at store.py:105-113). A row whose `path` never got written would be unusable, hence the single transaction. `enabled` defaults to 1, `last_hunt_*` and `deleted_at` NULL (schema.sql:5-26). Then `log_event("repo", f"added {name} ({forge}) -> {repo_path}")` with `repo_path = repo_dir(cfg.work_root / 'repos', rid)` (server.py:660-661); Rust builds the same message from `Store::repo_dir` (server.rs:1090-1101).

**Success**: `201 {"ok": true, "repo": <get_repo(rid) row>}` (server.py:662; server.rs:1102-1107) — the §6 shape: every `repos` column except `deleted_at`.

**NO git clone happens here.** The clone is deferred to the scheduler:
- `pick_next` selects kind `hunt` for a repo whose `path` doesn't exist ("not cloned yet -> hunt does the clone", scheduler.py:1936-1937).
- `run_hunt`: `rpath.parent.mkdir(parents=True, exist_ok=True)` then `run_cmd(["git", "clone", "--", repo["url"], str(rpath)], timeout=600)` (default cwd); rc != 0 -> `log_event("error", f"hunt {rname}: clone failed: {out[-300:]}")` and cycle summary `{"error": f"clone failed: {out[-300:]}"}` (scheduler.py:157-164). `run_recheck` has the identical clone block with message prefix `recheck #{fid}:` (scheduler.py:388-399). Other kinds refuse with `log_event("error", f"{kind} {rname}: repo not cloned")` / `{"error": "repo not cloned"}` (scheduler.py:585-587, 725-727).
- Path layout: the clone lives at `<work_root>/repos/repo-<id>`; the repo's notes live at `<work_root>/notes/repo-<id>.md`, beside the clones rather than inside one (`repo_dir` store.py:105-113 / store.rs:749-751, `repo_notes_path` store.py:703-727 / `Store::notes_path` store.rs:772-774). Notes are outside the checkout because a file inside it is untracked in a real git repo — a worker doing a broad `git add` commits the operator's private notes into a pull request — and because creating one used to create `repos/repo-<id>/` as a side effect, a directory with no `origin` that `sync_repo` reads as an already-cloned repo and refuses to work in, which made adding a note before a repo's first cycle enough to make it uncloneable for good. Both paths are keyed by id, never by name: names differing only in case are one directory on NTFS and APFS, Windows reserves `CON`/`NUL`/`AUX` and strips trailing dots, and anything past 255 bytes is `ENAMETOOLONG` raised at clone time rather than at add time. The name never reaches a path, so traversal is structurally impossible rather than filtered, and renaming a repo moves no files. Migration 007 rewrote the deployed rows off the historical `repos/<name>` layout; notes still sitting at the old in-clone path are moved by `migrate_repo_notes` (server.py:794-869; server.rs:1181-1293) — once at daemon startup in Rust (daemon.rs:200), before every cycle in Python (server.py:503, 1271), a no-op once nothing is left to move. It never overwrites — a note already at the new path is the current one.

**UI** (`ui-svelte/src/pages/ReposPage.svelte:136-168`): sends `{name, url, branch}` with `branch` defaulting to `"main"` client-side too, and adds `forge` only when the select is non-empty (`if (newForge)`, :146) — the auto-detect option's value is `""`, so the key is dropped and the server runs step 5. Success is `r.ok`, not a literal `201` (see `api.svelte.ts:20-28` on why comparing to a literal has already shipped as a bug): failure -> `console.error` plus `toast("Failed to add repo", false)` and the dialog stays open; success -> `toast("Added <name>", true)`, the four form fields reset, `store.refresh()`, and the caller closes the dialog (:321).

---

## 8. POST /api/repo/delete — delete repo (guarded)

Handler `_delete_repo` (server.py:664-699; server.rs:1317-1376).

**Request**: `{"id": int}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:667-669; server.rs:1324).
2. Look the repo up: `get_repo(rid)` / `get_repo_by_id(rid)` both filter `deleted_at IS NULL` (store.py:585-590), so this asks "is there an **active** repo?", and a miss splits two ways (server.py:671-684; server.rs:1329-1344):
   - **The row exists but is flagged** (`repo_is_deleted`, store.py:671-680) -> `200 {"ok": true}`, after re-attempting phase two (below). The repo really is deleted — it left every read path the moment it was flagged — so answering "no repo" would be a lie told to a client that is right. This covers both a delete whose reclamation failed and a delete whose response was lost in flight; the retry costs nothing and usefully re-attempts reclamation. That attempt's own failure is ignored exactly as it is on the first pass: the status is `200` either way. Nothing else happens on this path — no second `log_event`, no re-flagging, so repeating the request cannot alter `deleted_at` or the name suffix.
   - **No row at all** -> `404 {"error": "no repo <rid>"}`. Once reclamation succeeds the row is gone and the id is indistinguishable from one that was never issued, so a repeat of a *fully completed* delete 404s, as does an id that was never a repo. The `200` window is exactly the lifetime of the flagged row.
3. `soft_delete_repo(rid)` counts `SELECT COUNT(*) FROM findings WHERE repo_id = ?` and `SELECT COUNT(*) FROM jobs WHERE repo_id = ?`; if either > 0 -> **`400`** with exactly: `repo <rid> has <n> finding(s) and <m> job(s) -- cannot delete without losing history; pause it instead` (store.py:620-669). Python raises `ValueError` and the POST dispatcher renders it as `400 {"error": <message>}` (server.py:452-453); Rust maps `StoreWriteError::Refused` to the same body. Both counts and the flag below share one transaction, opened with `BEGIN IMMEDIATE` because the counts are reads and sqlite3 starts a transaction lazily on the first *write* (store.py:634-643), so a job created concurrently cannot land between the check and the flag.
   The count stays true after the flag: no job can be created in a flagged repo (`insert_job`), and no finding can be filed in one (`Store::upsert_finding`, Rust only, `FindingInsertError::RepoDeleted`). The second matters for work with no job row, the zero-token Renovate scan, which can be running when the delete lands: its findings would otherwise be invisible and, with no cascade on `findings.repo_id`, keep phase two from ever dropping the row.

**Deletion is two-phase, in both daemons.** `repos.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` (API-CONTRACT §7), so a freed id is never handed out again — outside the bounded window right after the rebuild migration, where ids above the highest surviving row are reissued until the counter passes the old high-water mark (API-CONTRACT §§7, 11) — and the phases are not ordering the *id* — they are ordering the *directory*. `<work_root>/repos/repo-<id>` is keyed by the id, removing a multi-gigabyte clone is not instant, and the flagged row is the only record that those files are still there. The row outlives the files because dropping it first would leave a directory nothing owns — and `sync_repo` treats any directory at that path as an existing clone.

- **Phase one (synchronous, inside the request)**: `UPDATE repos SET deleted_at = <now>, name = name || ' (deleted #' || id || ')'` (store.py:660-665). Every repo read path filters `deleted_at IS NULL`, so the repo is gone from the API immediately. The name is released here, not in phase two: `repos.name` is `UNIQUE`, and a flagged row would otherwise keep rejecting the name of a repo the operator has already been told is gone.
- **Phase two (`reap_repo`, server.py:760-791; server.rs:1120-1163)**: remove `<work_root>/repos/repo-<id>` entirely — clone and any worktrees under it — then the notes file `<work_root>/notes/repo-<id>.md`, which lives outside the clone and so survives that removal, and only then `DELETE` the row (`forget_deleted_repo`, itself `WHERE deleted_at IS NOT NULL`). That delete is the moment the repo stops being tracked at all, so it must not run first: a removal that fails or is interrupted has to stay findable. A missing directory and a missing notes file are both success; any other error leaves the row flagged for the next pass. The notes file is removed rather than left behind: nothing else will ever revisit it, and it is the operator's private context for a repo that no longer exists. **`reap_repo` takes the notes lock itself, in both daemons** (server.py:774; server.rs:1134) — a note written between the removals and the row delete recreates a file for a repo with no row left to retry it, and there are two callers to cover: the inline attempt in the delete handler and the `reap_deleted_repos` pass (server.py:872-885; server.rs:1296-1315), which the handler does not wrap.

Phase two is attempted inline so the common case completes before the response, but **its failure is not the caller's failure**: the response is still `200 {"ok": true}`, a warning naming the path is logged, and the row simply stays flagged until a reaper pass retries — before every cycle in Python (server.py:507 and 1275); in Rust at daemon startup and on every run of the admin lane's reaper duty (`AdminTask::Reap`, every 5 minutes, beside the job lane). A crash between the phases is safe for the same reason. Until that retry succeeds, step 2's first branch is what the endpoint answers for this id.

**Then**: exactly one `log_event("repo", "deleted <name> (#<rid>)")`, written after phase one (server.py:688; server.rs:1349-1358). One deletion, one event — pinned by `deleting_a_repo_logs_exactly_one_event`. The step 2 retry path logs nothing, so a client retrying a lost response does not double the event either.

**Success**: `200 {"ok": true}` — no embedded object (server.py:699; server.rs:1375). Identical body for a first delete and for a retry against a flagged row; the two are distinguishable only by the log.

**UI** (`ReposPage.removeRepo`, `pages/ReposPage.svelte:116-133`): `confirm('Remove repo "<name>"? This cannot be undone.')` first, then sends `{id}`. Branches on `r.ok`: success -> `toast("Removed <name>", true)` + `store.refresh()`; failure -> `console.error` with the status plus `toast("Failed to remove <name>", false)`. The refusal message of step 3 — the one that names the finding and job counts — is **not** surfaced: the toast is a fixed string, so an operator who hits the guard must read the daemon log or the console to learn why.

---

## 8.1 POST /api/repo/rehunt — queue a full re-hunt (Rust only)

Handler `request_rehunt` (server.rs). No Python counterpart.

**Request**: `{"id": int}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}`.
2. no active repo (`get_repo_by_id`, `deleted_at IS NULL`) -> `404 {"error": "no repo <rid>"}`. A delete that lands between the lookup and the write gives the same 404 (`Store::request_full_hunt` is itself `WHERE deleted_at IS NULL` and reports whether it matched).

**Write**: `UPDATE repos SET full_hunt_requested_at = MAX(<now>, full_hunt_requested_at + 1, full_hunt_request_attempted + 1)` (migration 020; NULLs count as 0). The value identifies the request — hunts compare it by equality — so it never repeats, even for two requests in the same millisecond. A pending request is overwritten, not kept: asking again after a chain has started queues another full pass instead of being absorbed by the one in flight. A paused repo accepts the request and keeps it until resumed. Then `log_event("repo", "full re-hunt queued for <name>")`, attributed (§10), and the scheduler is woken.

**Success**: `200 {"ok": true, "repo": Row}` — the refreshed repo, `full_hunt_requested_at` set.

**Scheduler semantics** (`scheduler::pick_next`, `scheduler::run_hunt`):
- Until a hunt has **attempted** the request, the rotation tier (below every finding tier and the resume tier) sorts the repo first and picks its hunt ahead of its other scans and of `scan.intervalDays`. A cold hunt records the request it read from the repo row (`repos.full_hunt_request_attempted`, migration 020) once it has created its job, or when it fails before that (a fetch error); a hunt the budget gate turns away has attempted nothing. Requests are matched by value, not time, so a request that lands while some other hunt is being prepared or run — one that read the repo row before it — stays unanswered. Once attempted, the request stops jumping the queue — a failing hunt is retried after the scan interval, like any failed rotation scan, not back to back — but stays pending.
- While pending, the next **cold** hunt reviews the complete history (`<empty tree>..<tip>`), exactly as the periodic `hunt.rehuntDays` re-hunt does, and never takes the "no new commits" skip. The chain records that scope and the request it accepted on its jobs (`jobs.full_history`, `jobs.full_hunt_request`, migration 020), and every resume of it inherits both.
- The request is cleared only when a full-history chain finishes `done` with no invalid findings **and** the pending request is the one that chain accepted (`Store::settle_full_hunt_request`). A request made or replaced while a chain is prepared or runs — full or incremental — survives it.

**UI** (`ReposPage.requestRehunt`): a "Full re-hunt" button per repo — "Re-queue re-hunt" while a request is pending — behind a `confirm(...)` whose text says which of the two it is; success -> `toast("Full re-hunt queued for <name>", true)` + `store.refresh()`; failure -> `console.error` with the status plus `toast("Failed to queue re-hunt of <name>", false)`. A pending request shows as a "full re-hunt queued" badge, read from `/api/summary.repos[].full_hunt_requested_at`.

---

## 9. POST /api/repo/notes — append a repo note (filesystem write)

Handler `_add_repo_note` (server.py:701-725).

**Request**: `{"id": int, "note": str, "category"?: str|null}`.

**Validation order**:
1. non-int id -> `400 {"error": "id must be an integer"}` (server.py:706-708).
2. `note` not a string or blank after strip -> `400 {"error": "note must be a non-empty string"}` (server.py:709-711).
3. `category` present, non-null, non-string -> `400 {"error": "category must be a string"}` (server.py:712-714).
4. no repo -> `404 {"error": "no repo <rid>"}` (server.py:716-719).
5. `category = category.strip() or None if category else None` (server.py:720) — empty/whitespace category becomes None.

**Filesystem write** `append_repo_note(rid, note.strip(), category)` (store.py:747-771), serialized on the class-level notes lock (store.py:754, `_NOTES_LOCK` store.py:161) against the reclamation that deletes the same file:
- Path: `<work_root>/notes/repo-<id>.md` (`repo_notes_path`, store.py:703-727; Rust `Store::notes_path`, store.rs:772-774); `mkdir(parents=True, exist_ok=True)` on `<work_root>/notes`. Outside the clone, for the reasons §7 gives. `repo_notes_path` re-validates the id and raises `ValueError` for an unknown repo (store.py:723-726) — unreachable from this endpoint, where step 4 has already 404'd it.
- If the file doesn't exist, create with header: `f"# Notes: {repo['name']}\n\nLast updated: {today}\n\n"` with `today = datetime.now(tz=UTC).date()` — renders `YYYY-MM-DD`, **UTC**; the "Last updated" line is written once at creation and never refreshed (store.py:759-764).
- Append (open mode `"a"`): if category, `f"## {category}\n"`; then `f"- [{ts}] {note}\n\n"` with `ts = datetime.now(tz=UTC).strftime("%Y-%m-%d %H:%M")` (**UTC**, minute precision) (store.py:767-771). Separator between entries = the trailing blank line. Category tag is a fresh `## heading` line per note (no dedup across consecutive same-category notes).
- No *file* locking. The notes lock is a process-level mutex — `Store._NOTES_LOCK` (store.py:161), `AppState.repo_notes` (server.rs:71), taken by the Rust handler at server.rs:1407 — so it orders appends against reclamation inside one daemon, which is all that is needed: `<work_root>/hunter.lock` admits one process (Appendix A). Two processes appending would rely on O_APPEND atomicity `[INFERENCE]`.

**Then**: `log_event("repo", f"note added to {repo['name']}" + (f" [{category}]" if category else ""))` (server.py:722-724).

**Success**: `201 {"ok": true, "notes": <string>}` (server.py:725) where notes is the **bounded re-read** `repo_notes(rid)` (store.py:731-745): full text if ≤ 4000 chars, else `"...(older notes truncated)...\n" + text[-4000:]` (`_MAX_NOTES_CHARS = 4000`, store.py:729).

**UI** (`ReposPage.addNote`, `pages/ReposPage.svelte:199-246`; the read side is `fetchRepoNotes`, `api.svelte.ts:262-273`): requires non-blank note text client-side (`toast("Note text is required", false)`, :201-205), sends `{id, note}` plus `category` only when non-empty (:210-214), and guards re-entry with a `notesSaving` set. Branches on `r.ok`, not on `201`: failure -> `console.error` + `toast("Failed to add note", false)`; success -> `toast("Note added", true)`, clear the form, **discard the response body**, `store.repoNotesCache.delete(id)` and re-`GET /api/repo/notes?id=N` via `store.fetchRepoNotes` (`lib/api.svelte.ts:262-273`), with its own failure toasted as `"Note saved, but reloading notes failed"` (:232-242). So the `notes` string in the 201 body has **no consumer in the shipped UI** — an earlier revision of this bullet called it the one success body that is read, which was true of the vanilla `app.ts` and is not true now. The GET side (`/api/repo/notes?id=N`, server.py:335-345) returns the same bounded string, which is what the panel actually renders.

---

## 10. POST /api/login, POST /api/logout, GET /api/me — sessions (Rust only)

Accounts are created by the operator (`hunter user add|passwd|disable <name>`, `main.rs`), never through the API.

- **`POST /api/login`** — not behind the session gate; behind the Host and Content-Type gates. Body `{"username": str, "password": str}`; a missing or non-string field -> `400 {"error": "<field> must be a string"}`. Usernames match case-insensitively (`users.username COLLATE NOCASE`). Success -> `200 {"username": "<stored spelling>"}` plus `Set-Cookie: hunter_session=<64 hex>; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000`, and a `sessions` row (expired rows are deleted on the way). A wrong password, an unknown name and a disabled account all -> `401 {"error": "invalid username or password"}` with no cookie; an unknown name is verified against a decoy argon2id hash so it takes as long as a wrong password (`auth::DECOY_HASH`).
- **`POST /api/logout`** — behind every gate. Deletes this session's row; `200 {"ok": true}` plus `Set-Cookie: hunter_session=; ...; Max-Age=0`.
- **`GET /api/me`** — `200 {"username": str}` (`SessionUser`).
- `hunter user passwd` and `hunter user disable` delete every session of the account in the same transaction.

**Attribution.** The handlers that log an event for an operator action (§1 verdict, §3 recheck, §4 unqueue, §5 override, §6–§9 repo writes, including §8.1, §11 hold release) record the session's account in `events.user_id` (`Store::log_user_event`); `/api/events` and finding timelines expose it as `username` (null for scheduler events).

## 11. POST /api/hold/release — end a hold (Rust only)

Ends the active hold on one kind of a finding's work (WORK-MODEL.md §9.3), or of a repo's (§17). Behind every gate.

**Repo subject**: `{"repo_id": int, "kind": str, "action": str}`, `kind` one of `hunt`, `test_gap`, `dep_update`, `refactor`, `modernization`, `standards`, `sync`. Both `repo_id` and `finding_id` -> `400 {"error": "exactly one of finding_id and repo_id"}`; `repo_id` not an int, or `kind`/`action` unknown -> `400`; no such repo -> `404 {"error": "no repo <id>"}`; no active hold, an action other than `continue`/`restart`, `continue` on `sync`, or `continue` without a checkpoint -> `409`. Writes (one transaction, `Store::release_repo_hold`): the hold row gets `released_at`, `released_by` and `release_action`, the work item's streak is reset to zero, `restart` clears its checkpoint, and a `release` event is attributed to the account. `continue` keeps the checkpoint, and the give-up ceiling counts from the release. Success: `200 {"ok": true, "repo": RepoOut}` (the repo as `/api/repos` serves it), and the scheduler loop is woken.

**Finding subject**:

**Request**: `{"finding_id": int, "kind": str, "action": str, "reason"?: str, "classification"?: str}`. `kind` is `recheck`, `fix`, `engage` or `harvest`; `action` one of `continue`, `restart`, `reject`, `wontfix`, `classify`, `mark_harvested`, `dismiss`. `reason` is the verdict's for `reject`, `wontfix` and `classify`; `classification` the closed PR's for `classify` (`superseded`, `duplicate`, `obsolete`, `wrong`, `unwanted`, `abandoned`, as `CLOSE-REASON.json` spells it). Absent, null, empty and whitespace-only strings are absent.

**Validation order** (first failure wins):
1. `finding_id` not an int -> `400 {"error": "finding_id must be an integer"}`; `kind` or `action` unknown -> `400`.
2. `reject`/`wontfix`/`classify` without `reason`, or `classify` without a known `classification` -> `400`.
3. No such finding -> `404 {"error": "no finding <id>"}`.
4. No active hold on that work, an action that does not apply to the kind (`reject`/`wontfix`: recheck and fix; `classify`: a harvest of a closed PR; `mark_harvested`: a harvest of a merged PR; `dismiss`: engage), or `continue` with no checkpoint to continue -> `409` with the reason. The `actions` the finding's `holds` list (API-CONTRACT.md §4) are exactly the ones that pass this.

**Writes** (one transaction, `Store::release_hold`): the hold row gets `released_at`, `released_by` (the session's username) and `release_action`, and a `release` event attributed to the account. Then: `continue` keeps the checkpoint, and the give-up ceiling counts only attempts started after this release (WORK-MODEL.md §8); `restart` clears the checkpoint; `reject`/`wontfix` set the finding's status and `verdict_reason` (and remove a verdict anchor), with a `verdict` event; `classify` does that with the classification's status (`Unclaimed::from(ClosureClass)`) and `"<class>: <reason>"`, and stamps the PR harvested; `mark_harvested` stamps the PR harvested and leaves the finding `merged`; `dismiss` marks the PR's current attention handled as a no-op engage would, and clears the engage's checkpoint and streak, so nothing resumes the dismissed engage. A verdict that moves the finding out of the kind's tier ends the work item (streak and checkpoint, migration 024's trigger). The scheduler is woken.

**Success**: `200 {"ok": true, "finding": Row}`.

## Appendix A. CSRF & concurrency guards

- **CSRF**: the `application/json` prefix gate on all POSTs (server.py:409-412, the HTTP 415 at :411) blocks HTML-form/simple cross-origin requests; combined with loopback-only bind `127.0.0.1` (server.py:903) and no CORS headers anywhere. GETs have no gate.
- **Process exclusivity**: exclusive non-blocking `flock` on `<work_root>/hunter.lock`, held for process lifetime; loser exits with a SystemExit message (server.py:65-87). Both `serve()` (server.py:922) and `daemon()` (server.py:1244) take it.
- **Cycle exclusivity**: `_cycle_lock` (server.py:93) — POST /api/cycle (server.py:490), daemon loop (server.py:1266-1299), summary's `cycle_running` probe (server.py:311).
- **Daemon wake**: `SchedulerHandle.wake` (the job lane's) is notified by `/api/cycle`, `/api/scheduler`, `/api/overdrive`, `/api/repo/rehunt`, and budget-override changes so the loop reacts without waiting for its current sleep deadline; `/api/cycle` and `/api/scheduler` also notify `SchedulerHandle.pr_sync_wake`, so the admin lane's PR sync duty reads open PRs at once, whatever its other duties are doing (Rust).
- **No per-row or per-table write locks**: every request builds its own `Store`/connection (server.py:157-160); serialization is SQLite-level (WAL, default 5 s busy timeout). Handlers are not atomic across their multiple store calls (§0.3); e.g. two concurrent verdicts on the same finding interleave at statement granularity — last `UPDATE` wins, both events logged.
- **Response-then-side-effect ordering**: /api/override writes the response *before* `_wake.set()` (server.py:590-592); /api/cycle responds 202 while the cycle thread runs concurrently (server.py:517-518). All other endpoints complete every write before responding.

## Appendix B. Success-body summary table

| Endpoint | Status | Body | Refreshing getter |
|---|---|---|---|
| /api/verdict | 200 | `{"ok": true, "finding": Row}` | `get_finding` (store.py:859-861) |
| /api/cycle | 202 / 409 | `{"started": true}` / `{"error": "busy"}` | — |
| /api/scheduler | 200 | `{"paused": bool}` | `/api/summary.scheduler_paused` |
| /api/overdrive | 200 | `{"overdrive": bool}` | `/api/summary.scheduler_overdrive` |
| /api/recheck | 200 | `{"queued": true, "finding": Row}` | `get_finding` |
| /api/unqueue | 200 | `{"ok": true, "finding": Row}` | `get_finding` |
| /api/override (all-clear) | 200 | `{"ok": true, "cleared": n}` | — |
| /api/override (per-finding) | 200 | `{"ok": true, "finding": Row}` | `get_finding` |
| /api/repo | 200 | `{"ok": true, "repo": Row}` | `get_repo` (store.py:585-590) |
| /api/repos | 201 | `{"ok": true, "repo": Row}` | `get_repo` |
| /api/repo/delete | 200 | `{"ok": true}` | — |
| /api/repo/rehunt | 200 | `{"ok": true, "repo": Row}` | `get_repo_by_id` |
| /api/repo/notes | 201 | `{"ok": true, "notes": str}` | `repo_notes` (store.py:731-745) |
| /api/hold/release | 200 | `{"ok": true, "finding": Row}`, or for a repo `{"ok": true, "repo": RepoOut}` | `get_finding` / `get_repo_by_id` |

All finding/repo Rows are `SELECT *` full rows — shapes per round-1 `hunter-rs/API-CONTRACT.md`. **The Svelte UI reads none of them**, including `/api/repo/notes`' `notes` string (§9): every write path calls `store.refresh()` or re-GETs instead. They are soft contract with no consumer today; ship them for parity.
