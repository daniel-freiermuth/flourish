# Hunter read-only API contract (Python -> Rust port, round 1)

Extracted from the Python source at commit state of 2026-09-13. Every claim cites `file:line`.
Sources: `hunter/hunter/server.py` (handler), `hunter/hunter/store.py` (SQL), `hunter/hunter/types.py` (types/config), `hunter/schema.sql` (DDL), `hunter/ui-svelte/src/` (consumer — the vanilla `hunter/ui/src/app.ts` this document was first written against no longer exists; `hunter/ui/` is now the gitignored Vite bundle built from `ui-svelte`, `.gitignore:15`).

Parity bar: the Svelte UI (`hunter/ui-svelte/src/`) must work unchanged. Nothing client-side validates against a schema: `lib/validate.ts` is a structural guard that checks only what a component dereferences without a `?.`, so some missing keys blank the whole dashboard and others degrade to a dash with no signal. Which is which is section 13, and that split is the contract — not the TypeScript types ts-rs generates from `src/types.rs` into `lib/generated/`, which `api<T>()` casts to without looking at the bytes (`lib/api.svelte.ts:32-45`).

---

## 1. Transport & global behavior

- `ThreadingHTTPServer` subclass `_Server` (server.py:888-897): `allow_reuse_address = True`, `request_queue_size = 64`, `daemon_threads = True` (server.py:915).
- Binds `("127.0.0.1", port or cfg.serve_port)` — **loopback only** (server.py:903-905). Port already in use (errno 98) -> process exits with an error message (server.py:906-914). Rust binds `serve.host` instead (default `127.0.0.1`; `daemon::run_daemon`), and answers only `Host` names in `serve.allowedHosts` plus loopback — see API-CONTRACT-WRITES.md §0.1.
- Handler class attrs (server.py:145-147): `server_version = "hunter/1"`, `protocol_version = "HTTP/1.1"` (keep-alive), `timeout = 15` (idle keep-alive connections closed after 15 s).
- Every response goes through `_send` (server.py:162-168) which sets exactly these headers:
  - `Content-Type: <ctype>`
  - `Content-Length: <len>`  (always set; enables keep-alive)
  - `Cache-Control: no-store`  (**on every response, including static files**)
  - plus stdlib defaults `Server: hunter/1 Python/x.y` and `Date`.
- **Rust: every `/api` route except `POST /api/login` needs a session** -> `401 {"error": "login required"}` without one (API-CONTRACT-WRITES.md §0.1, §10). Static files are public.
- **No CORS headers anywhere.** Same-origin only. (POST additionally requires `Content-Type: application/json`, server.py:409-412 — CSRF mitigation; out of scope here.)
- JSON responses: `_json` = `json.dumps(obj)` with default separators (`, ` / `: `), `Content-Type: application/json` — **no charset parameter** (server.py:170-171).
- Error envelope: `_error(status, message)` -> body `{"error": "<message>"}` as JSON (server.py:173-174).
- Catch-all: any exception inside `do_GET` -> logged, response `500 {"error": "internal error"}` (server.py:252-254). This matters: several "validation" failures surface as 500, not 400 (see /api/findings).
- Unknown GET path -> `404 {"error": "not found"}` (server.py:251).
- Methods other than GET/POST: stdlib `BaseHTTPRequestHandler` default -> `501 Unsupported method` HTML error page. HEAD is NOT implemented.
- One fresh `Store` (own SQLite connection) per request (`_store`, server.py:157-160). No request-level locking for reads.
- Process startup (`serve`, server.py:919-931): exclusive `flock` on `<work_root>/hunter.lock` (server.py:65-87); second instance exits.

---

## 2. Static file serving

Handled first in `do_GET` (server.py:200-215). UI files live in `UI_DIR = PROJECT_ROOT / "ui"` where `PROJECT_ROOT` is the directory containing the `hunter/` package tree, i.e. the repo's `hunter/` dir (types.py:17-20). On-disk contents are the Vite build output of `hunter/ui-svelte/`, and the whole directory is gitignored (`.gitignore:15`): `ui/index.html`, `ui/assets/index-<hash>.js`, `ui/assets/index-<hash>.css`. `index.html` references exactly those two hashed assets and carries its favicon as an inline `data:` URI (`hunter/ui/index.html:5,8-9`), so a build also drops unreferenced `icons.svg`/`favicon.svg` into `ui/` that nothing ever requests — which is the only reason the absence of `.svg` from the content-type map below has never surfaced as a 404.

| Path | Behavior |
|---|---|
| `/` or `/index.html` | Serve `UI_DIR/index.html` bytes, `200`, `text/html; charset=utf-8`. If missing: `404 {"error": "ui/index.html missing"}` (server.py:200-206). |
| any path NOT starting with `/api/` whose last `.`-suffix is in the type map | Serve `UI_DIR/<path-with-leading-slashes-stripped>` if it is a regular file **and** `UI_DIR in asset.resolve().parents` (path-traversal guard; symlinks resolved) (server.py:208-215). |
| anything else | falls through to `404 {"error": "not found"}` (server.py:251). Note: a non-`/api/` path with an unmapped/absent extension is 404 JSON, and a mapped extension whose file is missing or escapes UI_DIR is also 404 JSON. |

Content-type map `_STATIC_TYPES` (server.py:190-194):

| ext | Content-Type |
|---|---|
| `.js` | `application/javascript; charset=utf-8` |
| `.css` | `text/css; charset=utf-8` |
| `.map` | `application/json` |

No ETag / Last-Modified / Range support; `Cache-Control: no-store` on everything (server.py:166).
Suffix extraction: `url.path.rsplit(".", 1)[-1]` if the path contains a `.`, else no match (server.py:209-210). Query strings are ignored for static paths (urlparse splits them off, server.py:197).

---

## 3. GET /api/summary

Handler: server.py:216-217 -> `_summary()` (server.py:256-333) -> `_validate_summary()` (server.py:1059-1067).

- **Query params: none** (ignored).
- Success: `200`, a single JSON object of shape `SummaryDict` (server.py:1029-1053).
- `_validate_summary` runs `pydantic.TypeAdapter(SummaryDict).validate_python(payload)` right before serialization (server.py:1056-1067). A malformed payload raises -> `500 {"error":"internal error"}`. **Side effect to preserve: pydantic drops keys not declared in the TypedDicts.** Concretely, `repos` rows come from `SELECT *` and would contain the 6 migration-added repo columns (`last_full_hunt_at` … `last_standards_at`) — pydantic strips them, so `/api/summary`'s repo objects carry exactly the 10 `RepoDict` keys, while `/api/repos` (unvalidated) returns all columns. `[INFERENCE]` from pydantic's default TypedDict extra-key behavior; verify with a live diff if in doubt.

### Top-level shape (server.py:312-333, types at server.py:1029-1053)

| key | type | source |
|---|---|---|
| `backend_status_html` | string | `self.backend.status()` — **this is the key that carries the backend-status HTML fragment**; the UI injects it with `{@html}` (server.py:261,313; `pages/StatusPage.svelte:60`) and then rewrites any `<time data-ms>` element inside it into the browser's local time, which the server cannot know (`StatusPage.svelte:37-48`; the Rust facade emits that markup unescaped for exactly this, `backends/omp_scavenge/facade.rs:577-579,643`). Otherwise opaque; Rust port must expose the same key name. |
| `counts` | object: status -> int | `dict.fromkeys(FINDING_STATUSES, 0)` then `Counter` of `status` over `list_findings()` (server.py:262-264). **Every status key always present** (zero-filled; Rust: `FindingStatus::ALL`, `Store::status_counts`): `new, rechecking, queued, fixing, pr_open, merged, closed, superseded, rejected, wontfix, note`. The Python original had 9; `closed` (PR closed unmerged, awaiting its harvest) and `superseded` (the harvest found the work landed elsewhere, was a duplicate, or is obsolete) are Rust-only, and neither is suppressed. |
| `type_counts` | object: type -> int | `Counter` of `type` over the same rows (server.py:266,315). Only observed types present; may be `{}`. |
| `repos` | array of Repo | `store.list_repos()`: `SELECT * FROM repos WHERE deleted_at IS NULL ORDER BY name`, rows through `_repo_row` (store.py:592-596, 116-128), then pydantic-narrowed to `RepoDict` (types.py:143-158): `id:int, name:str, url:str, path:str, forge:str, default_branch:str, last_hunt_sha:str\|null, last_hunt_at:int\|null, enabled:int (0/1, NOT bool), added_at:int`. The narrowing drops the 6 `last_*_at` migration columns here; `deleted_at` is already gone before it (§7). Rust (`RepoBrief`) adds an 11th key, `full_hunt_requested_at:int\|null` (migration 020): the pending full re-hunt request the Repos page badges (`API-CONTRACT-WRITES.md` §8.1). It also carries the repo's work (WORK-MODEL.md §17), both keys always present: `holds`, an array of `{kind, reason, detail, held_at, actions}` — one per held work item, oldest first, `kind` a repo job kind or `sync` (`sync` held means nothing of the repo runs), `actions` what `POST /api/hold/release` accepts for it now (`continue` only with a checkpoint, `restart` always) — and `failing`, an array of `{kind, streak, last_failure}` for work failing without a hold yet. |
| `last_cycle` | Event object or null | first event with `kind == "cycle"` within `recent_events(limit=500)` (server.py:267-270). Event shape: see /api/events. |
| `cycle_running` | bool | `_cycle_lock.locked()` (server.py:311,326) — true JSON boolean. Rust: the job lane is inside a cycle (`SchedulerHandle.running`); the admin lane's work does not set it. |
| `scheduler_paused` | bool | process-local pause flag from `SchedulerHandle`; blocks new job-lane cycles but does not interrupt a running worker, and does not stop the admin lane (PR sync, reaping, the sweep). |
| `scheduler_overdrive` | bool | process-local overdrive flag from `SchedulerHandle`; when true every budget decision uses the backend's prioritized verdict. Resets to false on daemon restart. |
| `current_job` | Job object or null | `store.current_job()` (store.py:1243-1280), see below. |
| `next_candidate` | object or null | computed only when `current_job` is null AND `scheduler.pick_next` yields a candidate (server.py:276-308); see below. |
| `scheduler_state` | object or null | `store.get_scheduler_state()`: `SELECT * FROM scheduler_state WHERE id = 1` (store.py:1295-1301). Shape (types.py:109-114): `id:1, state:str ("idle"\|"denied"\|"error"; the Rust daemon also writes "offline"), detail:str, next_wake_at:int\|null (epoch ms), updated_at:int`. Null until the daemon loop has run once. |
| `activity_status` | tagged union on `kind` | `_activity_status(...)` (server.py:1070-1138); see below. |
| `admin_activity` | object or null | Rust only: the admin lane's current task, `{task: "pr_sync"\|"reap"\|"fetch_ahead"\|"dependency_scan"\|"sweep", started_at: int}` (epoch ms), from `SchedulerHandle.admin` (`activity::AdminStatus`, set only for the length of the task); null between its passes. It runs beside the job lane, so it can be non-null whatever `activity_status` says. |
| `job_activity` | object or null | Rust only: the job lane's current phase outside its worker, `{phase: "cloning"\|"fetching"\|"preparing_tree"\|"publishing"\|"ingesting", started_at: int}` (epoch ms), from `SchedulerHandle.job` (`activity::JobStatus`, set by the helper doing the work for its length, `activity::job_phase`); null while the job lane picks, gates, runs a worker or sleeps. It says what a `working` status is doing, and what a `running` job does after its worker (tree, publish, ingest). |

### current_job (store.py:1243-1280)

SQL: `SELECT j.*, r.name AS repo_name FROM jobs j JOIN repos r ON r.id = j.repo_id WHERE j.state = 'running' ORDER BY j.id DESC LIMIT 1`.
If `finding_id` is set and the finding exists, two extra keys are added: `finding_summary` (string|null) and `finding_fingerprint` (string|null) — **absent otherwise, not null** (store.py:1254-1258; types.py:139-140 `NotRequired`).
JobDict keys (types.py:117-140): `id:int, kind:str, repo_id:int, repo_name:str, finding_id:int|null, state:str, pid:int|null, session_file:str|null, cap_tokens:int|null, tokens_new:int|null, calls:int|null, exit_code:int|null, killed_reason:str|null, notes:str|null, started_at:int|null, finished_at:int|null, model:str|null, usage_delta:float|null` (+ the two optional finding_* keys).

### next_candidate (server.py:276-308, TypedDict server.py:970-983)

Built from `scheduler.pick_next(store, cfg)` (exceptions swallowed -> null) and `backend.decide(anticipated_tokens=...)`; verdict = `outlook.prioritized` when the finding has `budget_override` set or scheduler overdrive is enabled, else `outlook.normal`.

| key | type | source |
|---|---|---|
| `kind` | string | job kind from pick_next |
| `id` | int | target row id |
| `label` | string or null | `target.get("summary") or target.get("name") or target.get("fingerprint")` (server.py:300-302) |
| `is_finding` | bool | `kind in ("engage","harvest","recheck","fix")` (server.py:285) |
| `is_prioritized` | bool | true when the finding has a budget override or scheduler overdrive is enabled |
| `budget_state` | string | `"denied"` or `"allowed"` — only these two values are ever emitted (server.py:292-296) |
| `budget_reason` | string | `Denied.reason` / `Granted.reason` |
| `budget_retry_at` | number or null | `Denied.retry_at` (epoch ms, float) when denied; always null when allowed (server.py:292-296) |

### activity_status (server.py:1070-1138; variant TypedDicts server.py:986-1026)

Discriminated union on `kind`, priority order:
1. `{"kind":"running","job":<JobDict>}` — when `current_job` non-null
2. `{"kind":"working"}` — `cycle_running` true, no job row: the job lane is picking, or preparing a job (fetch, clone, a Renovate scan) or finishing one (push, PR, ingest); which of those, `job_activity` says. PR sync is not part of it in Rust (`admin_activity`).
3. `{"kind":"error","detail":<scheduler_state.detail>}` — `scheduler_state.state == "error"`
4. `{"kind":"offline","detail":<scheduler_state.detail>}` — `scheduler_state.state == "offline"`: the last cycle was denied by the network gate, and `detail` is `"<host> unreachable"` (Rust addition; no Python counterpart)
5. `{"kind":"paused","candidate":<NextCandidateDict>}` — candidate present with `budget_state == "denied"`
6. `{"kind":"ready","candidate":<NextCandidateDict>}` — candidate present otherwise
7. `{"kind":"idle"}` — scheduler_state present, none of the above
8. `{"kind":"warming_up"}` — nothing at all (no cycle has ever run)

Variants carry ONLY the listed fields (no `job: null` on non-running variants). `StatusPage` switches on `activity_status.kind` with one branch per variant and no fallback (`pages/StatusPage.svelte:83-151`), and `validate.ts` lets an unknown kind through (`hasActivityFields` returns true by default, pinned by `validate.test.ts` with `"hibernating"`). So a **new** variant does not break the dashboard, but that state renders an empty activity card until StatusPage gains an arm for it, and a **renamed** variant looks exactly the same from the client: the old arm stops firing and the new name falls through to the same empty card. Only a variant that keeps its name but loses a field its arm reads is louder — `validate.ts` rejects the whole summary, and with it the other four polled responses (section 13.2). So adding or renaming a variant is a UI change to make alongside it, not a crash; dropping a field is a crash.

---

## 4. GET /api/findings

Handler: server.py:219-220 -> `_findings(qs)` (server.py:347-382).

### Query params (all optional; empty string == absent, server.py:349-352)

| param | type | default | behavior |
|---|---|---|---|
| `status` | string | none | SQL `status = ?`. Not validated against the enum — an unknown value just matches nothing. |
| `repo` | string | none | all-digits -> repo id lookup, else name lookup (`get_repo`, store.py:585-590). Python: **unknown repo raises ValueError -> `500 {"error":"internal error"}`** (server.py:354-359 + 252-254), not 400. **Rust deviation — `400 {"error":"unknown repo <key>"}`** (`ApiError::BadRequest`, server.rs:361): a bad query param is the caller's mistake, and a 500 both lies about whose fault it is and hides the reason behind the generic body. |
| `severity` | string | none | minimum severity; case-insensitive `low\|medium\|high` (types.py:44-63). Expands to `severity IN (...)` of all values at-or-above. Python: **invalid value -> ValueError -> 500** (same catch-all). **Rust deviation — `400 {"error":"invalid severity <value>"}`** (server.rs:365-366), same reasoning. |
| `type` | string | none | SQL `type = ?` on `findings.type`. |
| `unified` | string | `"1"` | **Python: not read** — `_findings` always calls the one `list_findings`, which applies the `type` filter and adds `category` (server.py:362-367). Rust still honours it: any value other than `"1"` drops the `type` filter and the `category` key (server.rs:329,369-375,386). The UI never sends it. |

### Response: `200`, JSON array (no pagination, no LIMIT)

Store: `list_findings` (store.py:863-911): `SELECT * FROM findings [WHERE status = ? / repo_id = ? / type = ? / severity IN (…)] ORDER BY id DESC`. Post-processing adds a computed `category` key per row (store.py:897-909):

| row `type` | `category` |
|---|---|
| `bug` | `bug_class` value |
| `dep_update` | `update_type` value |
| `test_gap` | literal `"coverage"` |
| `refactor` | `smell_type` value |
| `modernization` | `modernization_class` value |
| `standards` | `standard_section` value (`Finding::category()` types.rs:144-153; Python store.py:908-909) |
| anything else | **key absent** |

(Python no longer has a legacy query: the old `list_findings`/`list_all_findings` pair is now the single `list_findings` above. Only Rust keeps the legacy mode, behind `unified`.)

Handler then embeds per-row (server.py:369-381):
- `timeline`: **always added**, array of event rows for that finding, ascending id order. Store: `events_by_finding(fids)` (store.py:1375-1389): `SELECT * FROM events WHERE finding_id IN (…) ORDER BY id`, grouped by `finding_id`. Empty array when no events.
- `needs_attention`: added **only** for rows with `status == "pr_open"` that have a `pr_state` row (`get_pr_state`, store.py:1085-1087); value is `pr_state.needs_attention` (string or null). Key **absent** for all other rows.
- `holds` (**Rust only**): **always added**, `[]` when none. The finding's active holds (WORK-MODEL.md §9), at most one per kind, oldest first (`Store::active_holds`): `{kind: "recheck"|"fix"|"engage"|"harvest", reason: "prerequisite"|"stuck"|"gave_up", detail: str, held_at: int (ms), actions: [str], new_activity: bool}`. `new_activity` is set for an engage hold whose PR has had activity since it was held (`pr_state.last_activity_at > held_at`); it does not release the hold. `detail` is the worker's `BLOCKED.md`, or the failure key and its output; it is never copied into `verdict_reason`. `actions` lists what `POST /api/hold/release` accepts for it right now, in the order `continue, restart, reject, wontfix, classify, mark_harvested, dismiss`; `continue` only while the work has a checkpoint. A hold never changes `status`.
- `follow_up_of` / `follow_ups` (**Rust only**): **always added**. The finding whose engage/harvest job filed this one via `FOLLOW-UPS.json` (int or null), and the findings jobs on this one filed that way (array, ascending id, `[]` when none). Derived as `findings.found_by_job -> jobs.finding_id` (`Store::follow_up_pairs`), so findings ingested before migration 011 have neither.

### Row columns (findings table verbatim; schema.sql:28-91 + migrations, see section 11)

`id:int, type:str, repo_id:int, fingerprint:str, file:str|null (in practice "" default at insert, store.py:810), symbol:str|null, line:int|null, severity:str ("high"|"medium"|"low"), confidence:float, summary:str, detail:str|null, status:str, pr_url:str|null, created_at:int, updated_at:int, bug_class:str|null, evidence_plan:str|null, introduced_by:str|null, rung_achieved:int|null, verdict_reason:str|null, budget_override:str|null ("once"|"exempt"|null), ecosystem:str|null, package:str|null, current_version:str|null, latest_version:str|null, update_type:str|null, security_advisory:str|null, missing_tests:str|null (JSON-encoded array stored as TEXT — served as a string, NEVER json.loads'd), test_file:str|null, smell_type:str|null, suggested_refactor:str|null, modernization_class:str|null, current_approach:str|null, proposed_approach:str|null, standard_section:str|null` + computed `category`, `timeline`, conditional `needs_attention`.

The table also carries `found_by_job` (migration 011, §11), and no findings response does: Rust names its columns and never selects it, and Python's `SELECT *` reads are filtered through `_public_finding`, which pops it (store.py:854-857, applied at :861 and :910). That provenance is served the other way round, as `produced_finding_ids` on `/api/jobs` (§6), and folded into `follow_up_of`/`follow_ups` above.

---

## 5. GET /api/finding

Handler: server.py:222-228 -> `_finding_detail(qs)` (server.py:384-403).

- Query param `id`: required, must be all digits. Missing, non-digit, or nonexistent finding -> **`404 {"error": "no such finding"}`** in all three cases (server.py:224-226, 393-399). (Never 400, despite the docstring.)
- Success `200`:
```json
{ "jobs": [ <job row>, ... ], "pr_state": { ... } | null }
```
- `jobs`: `jobs_by_finding(fid)` (store.py:1228-1241): `SELECT j.*, r.name AS repo_name FROM jobs j JOIN repos r ON r.id = j.repo_id WHERE j.finding_id = ? ORDER BY j.id DESC` — complete history, **no limit**. Columns: the 17 jobs columns (section 6) + `repo_name`.
- `pr_state`: `get_pr_state(fid)` (store.py:1085-1087): `SELECT * FROM pr_state WHERE finding_id = ?`; the whole row as an object, or `null` if none. Columns (schema.sql:164-207): `finding_id:int, pr_number:int|null, state:str|null (OPEN|MERGED|CLOSED), mergeable:str|null, checks:str|null, head_ref:str|null, last_activity_at:int|null, last_engaged_activity_at:int|null, needs_attention:str|null, attention_since:int|null, attention_fingerprint:str|null, addressed_fingerprint:str|null, head_sha:str|null, addressed_head_sha:str|null, synced_at:int|null, harvested_at:int|null`. The failure streaks are not on the wire: migration 023 moved them from `findings` and `pr_state` to `finding_work`.

---

## 6. GET /api/jobs

Handler: server.py:229-231. No query params (ignored).

Store: `list_jobs(limit=50)` (store.py:1200-1226):
```sql
SELECT j.*, r.name AS repo_name FROM jobs j
 JOIN repos r ON r.id = j.repo_id
 ORDER BY j.id DESC LIMIT 50
```
Response: `200`, JSON array. Row = all 17 `jobs` columns + `repo_name:str`:
`id:int, kind:str, repo_id:int, finding_id:int|null, state:str (queued|running|done|failed|killed|denied), pid:int|null, session_file:str|null, cap_tokens:int|null, tokens_new:int|null, calls:int|null, exit_code:int|null, killed_reason:str|null, notes:str|null, model:str|null, usage_delta:float|null, started_at:int|null, finished_at:int|null` (schema.sql:98-117).

Each entry carries one key that is not a `jobs` column: `produced_finding_ids: int[]` — the findings this job brought into existence. It comes from one correlated subquery, `(SELECT group_concat(f.id) FROM findings f WHERE f.found_by_job = j.id)` split on `,`, rather than a query per job: this feeds a 50-row table that polls, so an N+1 here would be 50 extra round trips every few seconds (store.py:1200-1226; store.rs:1259-1311).
**Always present.** `group_concat` returns NULL for a job that produced nothing — the common case, every fix, recheck and engage job — and both daemons render that as `[]`, never null and never an absent key, so a client cannot end up distinguishing "produced nothing" from "not reported" (types.rs:203-220). The subquery has no `ORDER BY`, so the order within the list is whatever the scan produced, not a promise.
The key is unique to this endpoint: `/api/summary.current_job` (store.py:1243-1280) and `/api/finding.jobs` (store.py:1228-1241) select the jobs columns alone.
Note: jobs whose repo row was deleted would vanish (INNER JOIN) — moot in practice because `soft_delete_repo` refuses while jobs reference the repo (store.py:620-669).

---

## 7. GET /api/repos

Handler: server.py:232-233; Rust `repos` -> `Json<Vec<RepoOut>>`: each row flattened with its `holds` and `failing`, as `/api/summary.repos` carries them (§3; WORK-MODEL.md §17). No query params.
Store: `list_repos()` — Python `SELECT * FROM repos WHERE deleted_at IS NULL ORDER BY name` with each row passed through `_repo_row` (store.py:592-596, helper at store.py:116-128); Rust `Store::list_repos` selects an explicit column list with the same `WHERE deleted_at IS NULL ORDER BY name` (store.rs:1006-1021). `ORDER BY name` is the default BINARY collation, so it is case-sensitive.
Response: `200`, JSON array of repos rows — schema columns `id:int, name:str, url:str, path:str, forge:str (github|gitlab), default_branch:str, last_hunt_sha:str|null, last_hunt_at:int|null, enabled:int (0|1), added_at:int` (schema.sql:5-18) **plus** the 6 migration columns `last_full_hunt_at:int|null, last_test_gap_at:int|null, last_dep_update_at:int|null, last_refactor_at:int|null, last_modernization_at:int|null, last_standards_at:int|null` (schema.sql:19-24; §11). Unlike `/api/summary.repos`, nothing strips *those*. Rust also serves the two columns of migration 020: `full_hunt_requested_at:int|null`, the time of a pending full re-hunt request (null when none is pending), and `full_hunt_request_attempted:int|null`, the request value a hunt last started to answer (`API-CONTRACT-WRITES.md` §8.1).

**`deleted_at` is not part of this shape**, in either daemon. The column exists on the table (schema.sql:25, §11) but it is bookkeeping for two-phase deletion, not API surface: Rust never selects it (`Repo` has no such field, types.rs:35-52), and Python's `SELECT *` would otherwise leak it, so `_repo_row` pops it (store.py:116-128). The filter makes the value uninteresting anyway — every repo read path is `WHERE deleted_at IS NULL`, so a flagged repo is absent from this array entirely rather than present with a timestamp, and the key could only ever have serialized as `null`. Stripping it keeps a rollback from changing the response shape. The same applies to `/api/repo`'s and `POST /api/repos`' embedded `repo` object, which come from `get_repo` through the same helper (WRITES §§6-7).

`repos.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` (schema.sql:9; hunter-rs migration 009, which rebuilds the table because AUTOINCREMENT cannot be added by ALTER TABLE, 009_repos_autoincrement.sql:32, :68-69). Ids therefore come from `sqlite_sequence` and only ever move forward: a freed id is never handed out again — with one bounded exception at the rebuild itself, where the counter restarts from the highest id *surviving* rather than the highest ever issued. Every id freed before the upgrade that sits above that floor is therefore handed out again, one per insert, until inserts pass the pre-rebuild high-water mark: reaping 3 and 4 while 1 and 2 survive means the next two repos are 3 and 4, not one repo and then safety (§11 spells out the arithmetic). Once the counter is past that mark it is a true high-water mark, and from then on a client still holding a stale id — a browser tab left open across the deletion — can only ever *miss*. It gets `404 {"error": "no repo <id>"}` from `/api/repo` and `/api/repo/notes`, and from `/api/repo/delete` either a `404` or, while the row is still flagged, an idempotent `{"ok": true}` that re-attempts reclamation (server.py:671-684; server.rs:1329-1343). What it can no longer do is pause, delete or annotate whichever repo would otherwise have inherited the number — a window that ordering the cleanup cannot close, because it is as long as the tab stays open (009_repos_autoincrement.sql:2-21). Two-phase deletion (WRITES §8) stays: it is what stops a half-removed directory being mistaken for a clone.

---

## 8. GET /api/repo/notes

Handler: server.py:235-237 -> `_repo_notes(qs)` (server.py:335-345).

| condition | response |
|---|---|
| `id` missing or not all-digits | `400 {"error": "id query param must be an integer"}` (server.py:337-339) |
| no repo with that id | `404 {"error": "no repo <id>"}` (server.py:342-344) |
| ok | `200 {"notes": "<string>"}` (server.py:345) |

Notes are **not in the DB**: read from `<work_root>/notes/repo-<id>.md` (`repo_notes_path`, store.py:703-727; Rust `Store::notes_path`, store.rs:772-774). Beside the clones, not inside one: the file used to live at `repos/repo-<id>/NOTES.md`, where a worker doing a broad `git add` would commit the operator's private notes into a pull request, and where writing the first note created `repos/repo-<id>/` as a side effect — a directory with no `origin`, which `sync_repo` then reads as an already-cloned repo and refuses to work in, permanently. Notes left at the old path are moved by `migrate_repo_notes` (server.py:794-869; server.rs:1181-1293): Rust runs it once at daemon startup (daemon.rs:200), Python before every cycle (server.py:503, :1271) — the same trigger split API-CONTRACT-WRITES.md §7 states. Missing file -> `""`. Files longer than 4000 chars are truncated to the **tail** 4000 chars with prefix `"...(older notes truncated)...\n"` (`repo_notes`, store.py:731-745, `_MAX_NOTES_CHARS = 4000` at store.py:729).

---

## 9. GET /api/events

Handler: server.py:238-240.
Store: `recent_events(limit=100)` (store.py:1368-1373): `SELECT * FROM events ORDER BY id DESC LIMIT 100`.
Response: `200`, JSON array of `{id:int, at:int (epoch ms), kind:str, message:str, job_id:int|null, finding_id:int|null}` (schema.sql:152-159; types.py:161-169). Rust adds `username:str|null`: the account an operator event was recorded for (API-CONTRACT-WRITES.md §10), null for the scheduler's own events.

---

## 10. GET /api/stats

Handler: server.py:241-250 — the response is the **complete** three-key object:
```json
{ "totals": {...}, "by_kind": [...], "by_finding": [...] }
```

### totals — `stats_totals()` (store.py:1588-1601), single object
```sql
SELECT COUNT(*) AS jobs,
 SUM(tokens_new) AS total_tokens,
 SUM(calls) AS total_calls,
 SUM(usage_delta) AS total_usage_delta,
 SUM(CASE WHEN state='done' THEN 1 ELSE 0 END) AS done,
 SUM(CASE WHEN state='denied' THEN 1 ELSE 0 END) AS denied
FROM jobs
```
Keys: `jobs:int, total_tokens:int|null, total_calls:int|null, total_usage_delta:float|null, done:int|null, denied:int|null`. With zero job rows: `jobs = 0` and every SUM is null (SQL semantics) — the UI types `done`/`denied` as nullable (`StatsTotals`, generated from `src/types.rs`) and formats them with `?? '–'` (`pages/StatsPage.svelte:38,43`), so null survives and renders as a dash; replicate SQL behavior exactly. (`rows[0] if rows else {}` — the else branch is unreachable for an aggregate.)

### by_kind — `stats_by_kind()` (store.py:1554-1570), array
```sql
SELECT kind, COUNT(*) AS jobs,
 SUM(CASE WHEN state='done' THEN 1 ELSE 0 END) AS done,
 SUM(CASE WHEN state='failed' THEN 1 ELSE 0 END) AS failed,
 SUM(CASE WHEN state='killed' THEN 1 ELSE 0 END) AS killed,
 SUM(CASE WHEN state='denied' THEN 1 ELSE 0 END) AS denied,
 SUM(tokens_new) AS total_tokens,
 SUM(calls) AS total_calls,
 AVG(tokens_new) AS avg_tokens,
 SUM(usage_delta) AS total_usage_delta,
 GROUP_CONCAT(DISTINCT model) AS models
FROM jobs GROUP BY kind ORDER BY kind
```
Row keys/types: `kind:str, jobs:int, done:int, failed:int, killed:int, denied:int, total_tokens:int|null, total_calls:int|null, avg_tokens:float|null, total_usage_delta:float|null, models:str|null` (comma-joined distinct model strings, e.g. `"opus,sonnet"`).

### by_finding — `stats_by_finding()` (store.py:1572-1586), array
```sql
SELECT j.finding_id, f.fingerprint, f.status, f.severity,
 COUNT(*) AS jobs,
 SUM(j.tokens_new) AS total_tokens,
 SUM(j.calls) AS total_calls,
 SUM(j.usage_delta) AS total_usage_delta
FROM jobs j JOIN findings f ON f.id = j.finding_id
WHERE j.finding_id IS NOT NULL
GROUP BY j.finding_id
ORDER BY total_tokens DESC
```
Row keys: `finding_id:int, fingerprint:str, status:str, severity:str, jobs:int, total_tokens:int|null, total_calls:int|null, total_usage_delta:float|null`. Note `ORDER BY total_tokens DESC` puts NULL totals last in SQLite (NULLs sort last on DESC).

---

## 11. Database DDL (schema.sql + in-code migrations)

`Store.__init__` (store.py:168): executes `schema.sql` as a script on every connection open (all `CREATE TABLE/INDEX IF NOT EXISTS`), then probes-and-applies these ALTERs for pre-existing DBs (list and loop at store.py:171-284; probe = `SELECT <col> FROM <tbl> LIMIT 1`, on OperationalError run the ALTER):

```sql
ALTER TABLE repos ADD COLUMN forge TEXT NOT NULL DEFAULT 'github';           -- also in schema.sql
ALTER TABLE jobs ADD COLUMN model TEXT;                                       -- also in schema.sql
ALTER TABLE jobs ADD COLUMN usage_delta REAL;                                 -- also in schema.sql
ALTER TABLE findings ADD COLUMN budget_override TEXT;                         -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_full_hunt_at INTEGER;                       -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_test_gap_at INTEGER;                        -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_dep_update_at INTEGER;                      -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_refactor_at INTEGER;                        -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_modernization_at INTEGER;                   -- also in schema.sql
ALTER TABLE findings ADD COLUMN modernization_class TEXT;                     -- also in schema.sql
ALTER TABLE findings ADD COLUMN current_approach TEXT;                        -- also in schema.sql
ALTER TABLE repos ADD COLUMN deleted_at INTEGER;                              -- also in schema.sql
ALTER TABLE findings ADD COLUMN proposed_approach TEXT;                       -- also in schema.sql
ALTER TABLE findings ADD COLUMN standard_section TEXT;                        -- also in schema.sql
ALTER TABLE repos ADD COLUMN last_standards_at INTEGER;                       -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN harvested_at INTEGER;                         -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN attention_since INTEGER;                      -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN attention_fingerprint TEXT;                   -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN addressed_fingerprint TEXT;                   -- also in schema.sql
ALTER TABLE findings ADD COLUMN fix_attempts INTEGER NOT NULL DEFAULT 0;      -- also in schema.sql
ALTER TABLE findings ADD COLUMN last_fix_failure TEXT;                        -- also in schema.sql
ALTER TABLE findings ADD COLUMN recheck_attempts INTEGER NOT NULL DEFAULT 0;  -- also in schema.sql
ALTER TABLE findings ADD COLUMN last_recheck_failure TEXT;                    -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN harvest_attempts INTEGER NOT NULL DEFAULT 0;  -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN last_harvest_failure TEXT;                    -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN head_sha TEXT;                                -- also in schema.sql
ALTER TABLE pr_state ADD COLUMN addressed_head_sha TEXT;                      -- also in schema.sql
ALTER TABLE findings ADD COLUMN found_by_job INTEGER;                         -- also in schema.sql
```

Every column in that list is *also* declared in `schema.sql`, so a fresh Python
install and an upgraded one converge on the same set of columns. The ALTERs
still matter: `schema.sql` runs `CREATE TABLE IF NOT EXISTS`, which does
nothing to a table that already exists, so on a pre-existing database the ALTER
list is the only thing that adds them. The Rust port's migrations must cover
both paths — `hunter/tests/test_schema_parity.py` asserts the two systems agree
from a fresh install *and* from an upgrade.

Three things do **not** follow that pattern:

- The partial index `repos_deleted_at ON repos(deleted_at) WHERE deleted_at IS NOT NULL` is **not** in `schema.sql`, and cannot be: `schema.sql` runs before the ALTERs, and on a pre-existing database `repos.deleted_at` does not exist yet at that point. Python creates it after the ALTER loop instead (store.py:321-324); hunter-rs creates it in migration 008 next to the column (008_repos_deleted_at.sql:21, :27-28) and again after the rebuild in 009, because dropping a table drops its indexes (009_repos_autoincrement.sql:104-105).
- `repos.id` is `INTEGER PRIMARY KEY AUTOINCREMENT` in `schema.sql` (:9), and AUTOINCREMENT cannot be added by an ALTER — it needs a table rebuild, so neither `CREATE TABLE IF NOT EXISTS` nor the probe-and-ALTER list can deliver it to a database that already has the table. Both daemons rebuild instead, and the two rebuilds are the same shape statement for statement: build `_repos_new`, copy every column, `DROP TABLE repos`, rename the new table *into* the referenced name (store.py:360-393; 009_repos_autoincrement.sql:68-100). Never the rename-first shape migration 003 uses on `findings`: `findings.repo_id` and `jobs.repo_id` reference `repos`, and renaming it out of the way first rewrites their REFERENCES clauses to point at a table that is then dropped. Ids are copied verbatim rather than reissued, because those two columns hold them (store.py:386-391). Both recreate the `repos_deleted_at` partial index the DROP took with them (store.py:395-398; 009:104-105), and both seed `sqlite_sequence` with the same guarded INSERT (store.py:405-409; 009:113-115). Python runs the rebuild behind a probe that strips comments out of `sqlite_master.sql` before looking for the keyword, so schema.sql's comment *explaining* AUTOINCREMENT cannot be mistaken for the declaration (store.py:42-50, :341-348) — on the deployed database, where migration 009 has already run, the probe does nothing. So this converges like everything else: fresh install or upgrade, Rust daemon or the Python rollback, whichever opens the database first, `repos.id` ends up non-reusable. Every id issued from that point on comes out of `sqlite_sequence`, which only moves forward, so a stale client reference can only miss rather than hit whatever repo now answers to it (§7).
  - One limit both implementations share, since neither comment says it outright: the seed is `COALESCE(MAX(id), 0)` over the rows that survived the copy, and `WHERE NOT EXISTS` makes it a no-op whenever the copy inserted anything — an explicit id on an AUTOINCREMENT table already raises the counter. The floor is therefore the highest id *still present*, not the highest ever issued, so every id freed before the rebuild that sits above that floor is handed out again, one per insert, until the counter passes the pre-rebuild high-water mark. Rebuilt from rows 1 and 2 after 3 and 4 were reaped, the next two inserts are 3 and then 4 — the window is as many inserts as there were freed ids above the floor, not one. Rebuilt from an empty table the next insert is 1: the empty case the seed exists for behaves exactly as it would with no sequence row at all, which is also the widest version of this window. Once inserts pass the old mark the counter is a real high-water mark again. The exposure is bounded to repos deleted before the rebuild; it is not ongoing.
- The partial index `findings_found_by_job ON findings(found_by_job) WHERE found_by_job IS NOT NULL` is not in `schema.sql` either, and cannot be, for the same reason as `repos_deleted_at`: on a pre-existing database `findings.found_by_job` does not exist when `schema.sql` runs. Python creates it after the ALTER loop, and after the findings rebuild that would drop it (store.py:538-548); hunter-rs creates it in migration 011 beside the column (011_findings_found_by_job.sql:24-25). Partial because the column is NULL for every finding ingested before provenance was recorded, and those are exactly the rows no job will ever be looked up by.

`findings.found_by_job` is **not API surface**, in either daemon — same treatment as `repos.deleted_at` (§7), and for the same reason: Python reads findings with `SELECT *` while Rust names its columns, so an internal column would otherwise land in one daemon's responses and not the other's. Python pops it in `_public_finding` (store.py:854-857); Rust's `Finding` has no such field. `/api/jobs` reports the same fact from the other side, as `produced_finding_ids` (§6). It is also not a foreign key to `jobs(id)`: jobs are pruned on a retention policy that knows nothing about findings, and a finding must outlive the job that found it rather than block its cleanup (011_findings_found_by_job.sql:19-21).

Also: `DROP INDEX IF EXISTS findings_fingerprint` cleanup on every open (store.py:291). PRAGMAs: `journal_mode = WAL`, `foreign_keys = ON` (schema.sql:2-3).

Tables (schema.sql): `repos` (:5-26), `findings` (:28-91 + 5 indexes :92-96), `jobs` (:98-117 + 2 indexes :118-120), `window_log` (:123-131), `calibration_samples` (:143-150), `events` (:152-159 + index :160), `pr_state` (:164-207), `scheduler_state` (:214-220, single row `CHECK (id = 1)`).

**There is no `repo_notes` table** — repo notes are Markdown files at `<work_root>/notes/repo-<id>.md`, one per repo, outside the clone directories (store.py:703-727; store.rs:772-774).

Tables the read-only API touches: `findings`, `repos`, `jobs`, `events`, `pr_state`, `scheduler_state`. (`window_log`/`calibration_samples` are backend-internal.)

---

## 12. Config the serve path needs (local `hunter/config.json`, loaded by `Config.load`, types.py:200-230)

Paths are resolved relative to `PROJECT_ROOT` (the `hunter/` dir containing the package) when not absolute (types.py:203-207).

| config key | Config field | default | used by serve path for |
|---|---|---|---|
| `serve.port` | `serve_port` | `8377` (types.py:187,223) | listen port; Python's bind host is hardcoded `127.0.0.1` (server.py:903) |
| `serve.host` | — (Rust: `serve_host`) | `127.0.0.1` | Rust only: bind address, an IP (`0.0.0.0` = every IPv4 interface); anything else is a load error |
| `serve.allowedHosts` | — (Rust: `allowed_hosts`) | `[]` | Rust only: `Host` names accepted beyond loopback (case-insensitive, no port); `["*"]` accepts any name and cannot be combined with names |
| `workRoot` | `work_root` | `"data"` | `hunter.lock` lockfile (server.py:75), repo clones `repos/repo-<id>` (store.py:105-113), repo notes `notes/repo-<id>.md` (store.py:727) |
| `dbPath` | `db_path` | `"data/hunter.db"` | SQLite database (store.py:165-166); Rust: also the daemon's `<dbPath>-daemon.lock` lockfile, beside `<workRoot>/hunter.lock` (`recovery::DaemonLock`), so two daemons with their own work roots cannot share a database |
| — (constant) | `UI_DIR` | `PROJECT_ROOT / "ui"` (types.py:20) | static files + index.html |

`hunter/config.anthropic.example.json` and `hunter/config.openai-codex.example.json` are tracked templates; copy the provider-matched one to ignored `hunter/config.json`. The templates bind compatible models to their `backend.llmProvider`. Other keys (hunt/fix caps, models, budget) feed the scheduler/backend, which `/api/summary` reaches only through `scheduler.pick_next` / `backend.decide()` / `backend.status()` — for round 1 those can stay behind a trait boundary.

---

## 13. UI consumer cross-check (`hunter/ui-svelte/src`)

Cite the symbol names below, not the line numbers: the numbers are a hint for finding them and nothing more. The first version of this section pinned every claim to a line in `hunter/ui/src/app.ts`, and when that file was replaced by the Svelte app the citations kept looking authoritative while pointing at nothing.

### 13.1 Every request the UI makes

| caller | request | what the response has to survive |
|---|---|---|
| `HunterStore.refresh` (`lib/api.svelte.ts:154-203`), on a 5 s timer (`#poll`/`startPolling`, `:205-220`) and again after every successful write | `GET /api/summary`, `/api/findings`, `/api/jobs`, `/api/events`, `/api/stats` — issued together in one `Promise.all` | **All-or-nothing.** Any non-200 among the five -> `error = "API error (non-200 response)"` and **no** state written; any body failing `lib/validate.ts` -> `error = "API error (malformed response body)"`, again nothing written; any transport failure, including the per-GET read deadline, -> `error` is the stringified exception, and still nothing written. One bad endpoint freezes the other four at their last good values behind App.svelte's staleness banner (`App.svelte:137-141`). |
| the same `refresh` | `GET /api/findings` carries **no query params** | Every filter, threshold and sort is client-side (`components/FilterBar.svelte:22-26,54-88`), so §4's query-param table has no UI consumer at all. |
| `HunterStore.fetchFindingDetail` (`api.svelte.ts:237-260`), lazily when a card is expanded (`FindingCard` renders `FindingDetail`, `components/FindingCard.svelte:280`; loader `components/FindingDetail.svelte:19-46`) | `GET /api/finding?id=<id>` | Kept only when `status === 200` **and** `isFindingDetail` passes; otherwise the cache entry is dropped and the panel shows "Failed to load finding detail". Refetched whenever a poll landed new data, since the cache is keyed to `store.revision`. |
| `HunterStore.fetchRepoNotes` (`api.svelte.ts:262-273`), lazily from `ReposPage.toggleNotes` on panel expand (`pages/ReposPage.svelte:170-197`) | `GET /api/repo/notes?id=<id>` | Throws unless `r.ok` **and** `typeof body.notes === "string"`; the caller collapses the panel and toasts. It does **not** read `error`. |
| `post()` (`api.svelte.ts:104-110`) from the page/component handlers; every GET above goes through `get()` (`:76-87`), which wraps `api()` (`:32-45`) with the abort deadline | the 9 POST routes | Section 14 and `API-CONTRACT-WRITES.md`. |

### 13.2 What is enforced at runtime — dropping any of this blanks the dashboard

`lib/validate.ts` is the only client-side check, and it is deliberately *not* a schema mirror: it checks what a component dereferences with no `?.`, plus every `{#each}` key, because Svelte 5 throws `each_key_duplicate` inside the component — past the store, where setting `error` could still have shown the operator a dashboard. Because the five polled responses are validated as a unit (§13.1), a violation anywhere in this list takes all of them down.

- `/api/summary` — `isSummary` (`validate.ts:83-99`): `activity_status` an object with a string `kind`; the field that `kind` implies — `job` object for `running`, `candidate` object for `paused` and `ready`, string `detail` for `error` and `offline`, nothing for the kinds whose branches render no payload; string `backend_status_html`; `counts` and `type_counts` objects; boolean `scheduler_paused` and `scheduler_overdrive`; `repos` an array of objects each carrying a **distinct numeric `id`**. Each `repos` row's `holds` and `failing`, when present and non-null, are rows with a distinct string `kind`, and each hold's `actions` an array with no repeated entry (`RepoWork` keys them).
- `/api/stats` — `isStats` (`:99-108`): `totals` an object; `by_kind` rows with a distinct **string** `kind`; `by_finding` rows with a distinct **numeric** `finding_id`.
- `/api/findings` — `isFindingList` (`:144-146`): rows with distinct numeric `id`, and each row's `timeline`, when present and non-null, rows with distinct numeric `id`.
- `/api/jobs` — `isJobList` (`:149-151`): rows with distinct numeric `id`, and `produced_finding_ids`, when present and non-null, an array with no repeated entry.
- `/api/events` — `isEventList` (`:154-156`): rows with distinct numeric `id`.
- `/api/finding` — `isFindingDetail` (`:130-136`): `jobs` rows with distinct numeric `id`; `pr_state` either absent/null or an object.

Nothing else is checked. `api<T>()` casts the parsed JSON to `T` without inspecting it (`api.svelte.ts:32-45`), so the generated types in `lib/generated/` state what the server is believed to send, never what the client verified.

### 13.3 Keys actually rendered — dropping one degrades silently

Absent here is a dash, an empty cell or a missing badge, with `error` still null and the staleness banner still hidden. That is the failure mode to weigh when deciding whether a key is droppable.

**`/api/summary`.** `backend_status_html` (`{@html}`, `pages/StatusPage.svelte:60`, plus the `<time data-ms>` rewrite at `:37-48`); `cycle_running` (button label and `disabled`, `:67-71`); `last_cycle.{at,kind,message}` (`:73-77`); `scheduler_state.{next_wake_at,detail}` (`:118-123,133-137,144-148,154-156`); `activity_status` payloads — `running` reads `job.{id,kind,repo_name,finding_id,finding_summary,finding_fingerprint,started_at,finished_at,tokens_new}` (`:84-92`), `error` and `offline` read `detail`, `paused`/`ready` read `candidate.{kind,id,label,is_finding,budget_state,budget_reason,budget_retry_at}` (`:105-137`, label via `candidateLabel`, `:31-33`); `repos[].{id,name}` build the repo-name maps on three pages (`pages/InboxPage.svelte:11`, `pages/AllFindingsPage.svelte:11`, `pages/KanbanPage.svelte:8`), and `repos[].{forge,enabled,url,default_branch,last_hunt_at,full_hunt_requested_at}` are rendered per card by `ReposPage` (`:344-366`) while `{url,added_at}` together form the identity that invalidates a repo's cached notes (`repoIdentity`, `ReposPage:67-68`). `repos[].holds[].{kind,reason,detail,held_at,actions}` and `repos[].failing[].{kind,streak,last_failure}` are rendered by `RepoWork` (`components/RepoWork.svelte`) under each repo on `ReposPage` and, for repos with holds, in `KanbanPage`'s Needs-you section, which also filters them by the repo filter and counts each such repo once.

**`/api/findings`.** `FindingCard` renders `id, type, severity, confidence, category, budget_override, needs_attention, pr_url, status, summary, fingerprint, file, line, timeline[].{id,at,kind,message}, detail, evidence_plan, verdict_reason` (`FindingCard.svelte:115-282`). `FilterBar` additionally reads `repo_id, type, category, severity, status, confidence` as filter dimensions and `created_at`/`updated_at` as sort keys (`FilterBar.svelte:22-26,54-87`); `KanbanPage` columns key off `status` plus `needs_attention` (`KanbanPage.svelte:11-16,31-32`).

**`/api/jobs`.** `LogPage` renders `id, started_at, finished_at, kind, repo_name, finding_id, produced_finding_ids, state, tokens_new, calls, model` (`LogPage.svelte:139-182`; `produced_finding_ids` is read through `produced()`, `:21-23`, which treats absent as `[]`).

**`/api/events`.** `LogPage` renders all six keys: `id, at, kind, message, finding_id, job_id` (`LogPage.svelte:89-108`).

**`/api/finding`.** `FindingDetail` renders `jobs[].{id,kind,state,tokens_new,started_at,finished_at,model}` (`:71-89`) and, when `pr_state` is non-null, `pr_state.{state,mergeable,checks,head_ref,pr_number,needs_attention,synced_at}` (`:104-127`).

**`/api/stats`.** `StatsPage` renders `totals.{jobs,total_tokens,total_calls,done,denied,total_usage_delta}` (`:23-49`), `by_kind[].{kind,jobs,done,failed,killed,denied,total_tokens,avg_tokens,total_usage_delta,models}` (`:75-87`) and `by_finding[].{finding_id,fingerprint,status,severity,jobs,total_tokens,total_calls,total_usage_delta}` (`:114-134`).

**`/api/repo/notes`.** `notes` only, rendered in a `<pre>` (`ReposPage.svelte:407-412`).

### 13.4 Shipped for parity, with no consumer to catch a regression

Serialized by the port and read by nobody in the Svelte UI. A wrong *value* in any of these is invisible; only the structural ones in the first group are load-bearing at all.

- Present-but-unrendered, still required by §13.2: `/api/summary`'s `counts` and `type_counts` must remain objects — no page displays either, so the counters are enforced as containers and ignored as data.
- Never read: top-level `next_candidate` (the status panel reads `activity_status.candidate` instead, which is the same payload by a different route), `candidate.is_prioritized`, `scheduler_state.{state,updated_at}`, `repos[].{path,last_hunt_sha}`.
- `/api/jobs` and `current_job`: `pid, session_file, cap_tokens, exit_code, killed_reason, notes, usage_delta`.
- `/api/finding`: `pr_state.{finding_id,last_activity_at,last_engaged_activity_at,attention_since}` and every pr_state column past those (`hunter-rs/src/types.rs:233-255`).
- `/api/stats`: `by_kind[].total_calls` — declared (`StatsByKind`) and given no column in the table.
- `/api/findings`: `symbol, bug_class, introduced_by, test_file` and the type-specific columns `ecosystem, package, current_version, latest_version, update_type, security_advisory, missing_tests, smell_type, suggested_refactor, modernization_class, current_approach, proposed_approach, standard_section`. The card shows the server-computed `category` (§4) in their place, and `category` *is* derived from them, so they are load-bearing on the server and inert on the wire.

### 13.5 The error envelope has no UI reader

No Svelte code reads `body.error`. Every caller branches on `r.ok` or `r.status` alone and reports failure in its own words — `console.error` plus a toast (`ReposPage.svelte:104-112`), or the store's own wording behind App.svelte's banner (the two literals at `api.svelte.ts:166,189` and the stringified exception at `:201`; `App.svelte:137-141`). The one place a status *code* carries meaning is `/api/cycle`, where 409 and 202 become different button text (`StatusPage.svelte:13-17`). Keep the `{"error": <msg>}` envelope and the 400/404/409/415/500 taxonomy — they are what a human debugging with curl sees, and the WRITES contract pins several of them — but expect no UI regression to fire if a message string changes.

---

## 14. Appendix: POST endpoints (out of scope for round 1)

All under the Axum router; all require `Content-Type: application/json` else `415`:
`POST /api/verdict` (set finding status), `POST /api/cycle` (trigger cycle; 202/409-busy), `POST /api/scheduler` (pause/resume), `POST /api/overdrive` (global prioritized budget mode), `POST /api/recheck`, `POST /api/unqueue`, `POST /api/override` (budget override incl. `id:"all"` clear), `POST /api/repo` (update), `POST /api/repos` (add, 201), `POST /api/repo/delete`, `POST /api/repo/notes` (append note, 201).
