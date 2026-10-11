# hunter — Idle-Token Bug Hunter

> The daemon is the Rust binary in `hunter-rs/`. This directory is its
> root: local `config.json`, `data/`, `playbooks/`, generated `ui/`, and
> `ui-svelte/`.

Self-hosted [detail.dev](https://detail.dev)-style code analysis pipeline
funded by spare Claude-subscription capacity. Register a repo; hunter finds
bugs, test gaps, outdated dependencies, mechanical refactors, and
modernization opportunities using headless `omp -p` workers; you triage
findings in a local web UI; fix workers ship draft PRs and follow up on
review feedback autonomously; a harvest pass reviews each merged or closed
PR's full lifetime for genuine deferred follow-up work. Runs as a daemon that idles at
zero token cost and only spends against a budget it never overshoots.

## Contents

- [Quick start](#quick-start)
- [How it works](#how-it-works)
  - [Finding types](#finding-types)
  - [Finding lifecycle](#finding-lifecycle)
  - [Scheduling loop](#scheduling-loop)
  - [Budget policy](#budget-policy)
  - [PR follow-up loop](#pr-follow-up-loop)
- [Web UI](#web-ui)
- [Users](#users)
- [API](#api)
- [Architecture](#architecture)
- [Data model](#data-model)
- [Configuration](#configuration)
- [Development](#development)
- [Running as a service](#running-as-a-service)

## Quick start

Requires a Rust toolchain and Node/npm (the daemon binary and the UI
bundle), `omp` on `PATH` (the headless worker CLI this project is built
around), `git`, and the `gh` CLI (or `glab` for GitLab repos)
authenticated for whatever repos you register.

```sh
cp hunter/config.anthropic.example.json hunter/config.json  # or config.openai-codex.example.json / config.github-copilot.example.json
(cd hunter/ui-svelte && npm ci)           # once per checkout, and after a dependency change
cd hunter-rs
just build                                # UI bundle (ui-svelte/ -> hunter/ui/) + release binary
./target/release/hunter --root ../hunter user add <you>  # your login (see § Users)
./target/release/hunter --root ../hunter  # run forever: UI (:8377) + scheduler loop
```

Then open `http://localhost:8377`, log in, go to **Repos**, and add a
repository (name, git URL, default branch). Everything else — registration,
triage verdicts, notes, pausing repos, manual cycle/recheck triggers —
happens in the UI; the binary itself takes only `--root`, `--port` and the
`user` commands (`USAGE` in hunter-rs/src/main.rs).

## How it works

### Finding types

| Type | What it looks for | Prompt |
|---|---|---|
| `bug` | Latent bugs — boundary, error-path, race, contract-drift, leak, logic | `playbooks/hunt.md` |
| `test_gap` | Missing test coverage for real code paths; untested code that already misbehaves is filed as a `bug` instead | `playbooks/test_gap.md` |
| `dep_update` | Outdated dependencies, one finding per Renovate branch (see below) | `playbooks/dep_update.md` |
| `refactor` | Conservative, safe, mechanical refactoring opportunities | `playbooks/refactor.md` |
| `modernization` | SOTA-drift — deprecated deps, language-feature gaps, format/protocol shifts, CI/CD gaps, platform EOL | `playbooks/modernization.md` |

All five share one `findings` table (see [Data model](#data-model)) and one
triage/fix pipeline; the UI's type filter and colored badges are the only
place the distinction usually matters day to day.

**Dependency updates** come from a zero-token Renovate lookup
(`--dry-run=lookup`, with `config:recommended` under the repo's own config)
whenever Renovate can read the repo, and from the `dep_update.md` worker
otherwise. A finding is one Renovate branch, i.e. one PR's worth of change:
packages released together (a monorepo group, the same tool pinned in a
manifest and in CI) are one finding, and a package with both an in-major and
a major update available gets one of each. The fingerprint is the branch
plus the versions installed today, never the target, so:

- a newer upstream release updates the open finding's target in place (only
  while it is `new` or `queued`);
- a merge or manual bump moves the installed version, which starts a new
  finding;
- a `new` finding the latest scan no longer proposes is set `superseded`,
  and returns to `new` if a later scan proposes it again. A dependency the
  scan skipped or failed to look up (e.g. GitHub Actions without a token)
  keeps its finding: not checked is not done.
- a `queued` finding the scan no longer proposes, whose move it proposes in
  another finding (same package or group member, same major/non-major
  class) -- e.g. one the AI fallback filed, or one from before grouping --
  is set `superseded`, and that finding is queued instead, keeping the
  budget override. Findings already in a PR are left alone.

### Finding lifecycle

```
new ──recheck──> new / superseded / rejected
 │
 ├─ queue for fix ─> queued ─> fixing ─> pr_open ─┬─> merged
 │                      ↑           │              └─> closed (closed unmerged)
 │                      └───────────┘ (requeued on a
 │                        recoverable failure, up to
 │                        a bounded same-reason streak
 │                        before giving up)
 │
 └─ triage verdict ─> wontfix / rejected / note
```

- **`new`** — freshly ingested, awaiting triage (or a `bug` fresh from a
  hunt; other types can also be queued directly for `apply_*` playbooks).
- **`queued` → `fixing` → `pr_open`** — a fix/apply worker verifies the
  finding and ships a draft PR, declines (`NOT-A-BUG.md`/`DECLINED.md`),
  or pauses. A requeued checkpoint continues in its original worktree and
  transcript instead of recreating the branch. A decline's first line
  classifies it with the closed-PR harvest's words and lands the same
  way: superseded / duplicate / obsolete → `superseded`, wrong →
  `rejected`, unwanted → `wontfix`; a decline that classifies nothing
  goes back to `new` for triage.
- **Held work** is not a status. When a fix worker writes `BLOCKED.md`
  (a verification prerequisite, dependency, or human decision is
  missing), or the same failure repeats three times in a row, the fix is
  *held* for you: the finding stays `queued`, the scheduler skips it, and
  the attempt is kept as its checkpoint (committed work, uncommitted
  reports, transcript) when a worker ran in it. It is NOT suppressed.
  Kanban's **Needs you** section lists it with the report or what kept
  failing, and the ways out: **Continue** resumes the checkpoint with a
  fresh give-up budget (a prerequisite hold asks the worker to re-evaluate
  it, a failure hold to finish the work), **Restart** starts over,
  **Reject**/**Wontfix** decide the finding. A recheck whose worker fails
  the same way three times is held the same way and stays `rechecking`,
  where it used to go back to `new` looking like a finding nobody had
  rechecked.
- **`pr_open`** — tracked via `sync_prs` (free — `gh pr view`, no worker)
  every cycle: merged → `merged`; closed unmerged → `closed`; new
  comments/reviews, `CHANGES_REQUESTED`, merge conflicts, or failing checks
  flag it `needs_attention` for an **engage** worker, which may also
  withdraw the PR (→ `closed`); the same worker session then carries on
  into the closed-PR harvest below when the window has room, else the
  harvest runs cold later.
- **`closed`** — the PR closed without merging. Deliberately NOT a
  rejection: of the first 16 closed PRs, 15 were engage withdrawals (14 of
  them superseded or obsolete) and one was closed by a human, so a closure
  on its own says little about whether the finding was wrong. A one-time
  **closed-PR harvest** reads the PR's diff, discussion and the current
  code, classifies the closure (`CLOSE-REASON.json`) and files what it
  left open (`FOLLOW-UPS.json`): superseded / duplicate / obsolete →
  `superseded` (valid, not suppressed), wrong → `rejected`, unwanted →
  `wontfix`, abandoned → `new`. A review that cannot classify it counts
  toward the harvest's failure streak; given up on, the finding stays
  `closed`.
- **`merged`** → queued for a one-time **harvest** pass reviewing the PR's
  complete lifetime (not just its open-time snapshot) for genuine deferred
  follow-up work, filed as new findings via `FOLLOW-UPS.json`.
- **`rejected` / `wontfix`** (with a required reason) become the
  **suppression corpus**: injected into every future hunt/analysis prompt
  for that repo+type so the same already-decided issue isn't re-proposed.
  A `rejected` suppression holds only while its reason does. A worker's
  rejection (a `wrong` decline or closed-PR harvest, an `invalid` recheck)
  states the condition it rests on (`holds while`) and the paths that
  condition lives in (`depends on`), and is anchored to the commit it
  judged (`verdict_anchors`). The prompt shows each entry's status, reason
  and condition, and marks an entry `CHANGED` once its paths differ
  between that commit and the scanned one. The scan then re-checks it:
  filing it again under the same fingerprint reopens the finding (`new`,
  with the new analysis); recording it in the scan's reconfirmation file
  (`<findings file>.reconfirmed.json`) moves the anchor to the scanned
  commit, so it is not re-checked on every run. `wontfix` and an
  operator's verdict are decisions, not claims about the code, so they
  have no anchor and stay unconditional. The limit is the worker's
  `depends on`: a change outside the named paths (finding 20's verdict
  rested on a trim in a file other than the finding's own) lapses
  nothing.
- **`note`** — informational, no action expected.
- **Recheck** (UI button, human-triggered only) re-evaluates a `new`
  finding against the *current* codebase with an adversarially skeptical
  second opinion: `confirmed` (refreshes the analysis, stays `new`),
  `stale` (→ `superseded`: the code moved on, so nothing is suppressed),
  `invalid` (→ `rejected`).

### Scheduling loop

One cycle picks exactly one unit of work, in strict priority order
(`scheduler.pick_next` — the single place this is expressed; the UI's
"what's next" preview calls the same function, so it can never drift from
what actually runs):

1. Any finding with an active **budget override** (any category) — human
   escape hatch, jumps the whole queue.
2. **Engage** the flagged PR with the oldest-outstanding attention reason.
3. **Harvest** the oldest merged or closed PR still pending its review
   (a closed PR qualifies only while its finding is `closed`, which no
   verdict can set; a finding a human moved on after the closure, even to
   `rejected`, is left alone. Closures from before the closed-PR harvest,
   which left their finding `rejected`, were moved to `closed` once by
   migration 016).
4. **Recheck** the oldest finding stuck in `rechecking`.
5. **Fix** the oldest queued finding.
6. **Resume** the newest suspended attempt that can still be continued.
   A worker killed for running out of window headroom keeps its session
   and comes back as `suspended` rather than `killed`, and the next cycle
   hands omp that exact session instead of starting cold. So does one
   that died after doing metered work — a provider error omp's retries
   did not absorb, a connection lost across a laptop suspend, or the
   daemon going down mid-job (found at the next startup). Never a
   wallclock overrun, which is the runaway signature, and never an
   attempt that failed before spending a token (a misconfigured model or
   an exhausted account fails the same way again).
   Ranked here because a suspension has already been paid for — it
   outranks *starting* new background work, never a human waiting on a
   PR. Restarting costs a flat ~37k-token session floor and then redoes
   the work; continuing costs re-caching the context the worker was
   already carrying.
7. Otherwise, **hunt/test_gap/dep_update/refactor** — whichever is most
   overdue for the least-recently-scanned enabled repo (a never-cloned repo
   always hunts first). **Modernization** joins this rotation too, but only
   once `modernization.intervalDays` (config, default 30) has passed since it last ran for that
   repo — a periodic strategic check, not a tight-loop scan competing for
   every cycle. A repo with a **full re-hunt** queued from the Repos page
   goes first, and its hunt runs ahead of its other scans and of the scan
   interval — until a hunt has attempted that request, so a failing hunt
   is not retried back to back.

Each of 1-5 picks a (finding, kind). When that finding has a suspended
attempt of that same kind that can still be continued, the tier continues
it, at the tier's own position, instead of starting the work fresh.

Before the picked work is dispatched, the cycle checks that this machine
can reach the hosts it needs: the repo's **forge** (the host of its URL,
e.g. `github.com` or the GitLab instance; none for a local clone) and the
**LLM provider's API** (`api.anthropic.com`, `chatgpt.com` or
`api.githubcopilot.com`, per `backend.llmProvider`). Every kind needs
both, since every kind can run a worker. The probe opens a TCP connection
to port 443 (or the URL's own port) on all of the host's addresses at
once, and one must accept within 2 seconds, lookup included; no request
is sent. If a host is unreachable, the cycle is denied with a
`deny` event naming the host and retried in 3 minutes: nothing runs,
nothing counts toward a failure streak or a rotation turn, and no job row,
claim or checkpoint is touched. The daemon runs on a laptop that travels
and sleeps, so being offline is a normal state, not a failure of the work
it would have started. The Status page shows such a cycle as **offline**,
naming the host, until a cycle gets past the gate.

The network can also go while work runs. So before a finding's failure is
counted, the same hosts are probed again: a forge call that failed before
the work started (a harvest's PR view or diff), a worker that failed
without doing any metered work, and publishing a finished worker's work
(a fix's push or PR). A finished attempt that could not publish is kept
as the checkpoint, so the next cycle publishes it rather than redoing it. If a host is unreachable then, the
failure is the machine's, not the work's: nothing is counted, a worker's
job is closed as never started, the checkpoint stays where it was, and
the cycle ends denied for the host like the gate's own denial. The probe
decides, never the error's text. With `network.gate: false` there is no
probe here either, and every failure counts.

Known limits of the gate:

- A captive portal (hotel or train Wi-Fi before its login page) accepts
  TCP on 443 for every host, so the gate passes and the work then fails.
  Re-probing a failure before it is counted (#94) passes there too, so
  those failures still count.
- A network that reaches the forge and provider only through a proxy
  never accepts a direct connection: set `network.gate: false` there.

The cost ceiling applies only when the historical typical estimate is
positive. An absent or zero estimate is not a zero-token allowance;
the attempt ceiling and provider budget still bound the run.

A rotation scan (tier 7) that does not succeed — the worker fails or is
killed, the job crashes, or it finishes without output or with any invalid
entry — still counts as that scan's turn: its `last_<kind>_at` moves to now,
so it is retried after its interval instead of being picked again at once
and starving every other repo. A failed hunt leaves its watermark where it
was, so the retry reviews the same commits. A suspended attempt is not a
turn taken; tier 6 continues it, and a chain abandoned at the give-up
ceiling counts as one.

A repeatedly-failing item (same failure reason, consecutive attempts) gives
up after a bounded streak rather than looping forever — this applies
uniformly to fix retries, recheck retries, and harvest retries. A resume
chain has the same kind of ceiling, on either of two counts: once it has
made four useful-work attempts, or once — after at least one resume —
everything it spent *besides its single largest attempt* is past three
times what that kind of job typically costs. Whichever fires, the
suspension is marked `failed` (reason `give-up`) instead of being
continued again. A chain
that has never been resumed is never retired: one attempt's spend is by
definition at least its own cap, and the largest attempt is discounted
because one enormous attempt says the job is big, not that the chain is
stuck. For fix, recheck and harvest a given-up chain is one failed
attempt in that finding's streak, so three given-up chains in a row end
the work like three identical failures; otherwise a worker that suspends
on every attempt would have its finding's tier start it fresh forever.
Engage has no streak: a given-up engage chain sets the PR's attention
aside, as an engage that did nothing would, until the PR's next comment,
review or push flags it again; it is logged as an `error` event with the
give-up reason, since the PR has left review. The ceiling bounds what
the scheduler continues on its own, so it counts only the attempts since
you last released the work: **Continue** on a held fix gives its chain a
fresh budget, and after that it is bounded like any other.
The dashboard's "what's next" preview makes the same selection without
any of these writes; the scheduler does them when it selects.

The initial attempt and every cap-killed attempt always count; any other
resumed handoff with zero or unknown metered spend (e.g. a provider failure
before the first response) does not use this allowance.

A finding's work continues from its work item's checkpoint, a pointer to
the attempt it resumes; job rows are records of attempts and are never
rewritten once they finish. Three more things end a checkpoint, because
after any of them it can never usefully be continued:

- **Superseded.** Starting the same work fresh — same finding and kind
  for engage/harvest/recheck/fix, same repo and kind for hunt and the
  analysis scans — ends the checkpoint in the same transaction that
  creates the fresh job, whichever path started it: a finding's work
  item drops its pointer, a repo scan's suspended attempt is marked
  `killed` (reason `superseded`).
- **Working directory gone.** Every kind runs in its chain's own
  workspace, `<work_root>/jobs/<origin_id>/`, and the transcript is full
  of absolute paths into it — `tree/`, the git worktree of the repo's
  clone, and `session/`, the transcript itself. A checkpoint is only
  resumable while all three of these still hold: `tree/` is there, its
  session file lies inside the chain's `session/`, and the row records
  the commit the tree was pinned to. Failing any of them is permanent —
  a row from before per-chain workspaces fails by construction, its
  transcript and tree being in the old layout and its row carrying no
  pinned commit — so selection gives the checkpoint up when it reaches
  it (a repo scan's attempt is marked `killed`, reason `workdir-gone`)
  and moves on to the next candidate the same cycle.
- **Finding moved on.** A finding's work exists only while the finding
  is in that kind's tier (fix: `queued`/`fixing`, recheck: `rechecking`,
  engage: `pr_open`, harvest: `merged`/`closed` and not yet harvested).
  Whatever moves it out — a verdict, an unqueue, the PR merging — ends
  the work item in the same write: its checkpoint and its failure streak
  go, and the next sweep releases the tree.

### Budget policy

`backends/omp_scavenge` reads omp's local usage mirror
(`~/.omp/agent/agent.db:usage_history`). Anthropic and OpenAI Codex jobs
are gated against two linear ramps:

- **7-day ramp**: allowed fraction = elapsed fraction of the week. Spreads
  spending evenly so a burst early in the week doesn't starve the rest of
  it. Scavenging stops entirely once the interactive-use reserve line is
  crossed (default 75%) — hunter never eats into headroom you need for your
  own interactive sessions.
- **5-hour ramp**: zero for a headroom period (default 30 min, tunable via
  `HEADROOM_MS` in `hunter-rs/src/backends/omp_scavenge/capacity.rs`) so a freshly-opened window is never
  immediately claimed, then ramps 0→1 over what's left. Unspent capacity at
  reset is wasted — there's no rollover.
- **GitHub Copilot**: premium-request usage follows one calendar-month
  ramp: allowed fraction = elapsed fraction of the actual UTC month
  (28–31 days). There is no 5-hour window. Unknown quota or an expired
  monthly observation denies work until refreshed; exhausted quota denies
  both normal and prioritized work. Accounts that report no premium-request
  limit are not treated as unlimited capacity. Copilot's chat and
  completions limits are not read or shown: they never gate hunter's jobs
  (unlimited on paid plans; the free plan has no premium quota).
- **Overdrive**: the Status-page toggle promotes every job to the prioritized
  budget path. It bypasses pacing and spends only the provider's
  remaining hard-limit headroom; an exhausted provider window is still denied.
  The toggle is process-local and resets to off when the daemon restarts.
- **In-flight accounting**: a running job's *anticipated* cost is reserved
  before the next decision, so concurrent/rapid cycles can't overshoot
  either ramp. It is an estimate from history, not the job's granted cap:
  a cold-cache first call arrives as one atomic, uninterruptible LLM call
  that has been observed spending 2-4x what was reserved for it. The
  history is the 20 most recent *completed chains* of that kind, not all
  of it — a window ages out both rows written under accounting bugs that
  have since been fixed and repos that have since changed size. A chain
  counts once, at what the whole chain cost: a resumed attempt only
  meters what it added, so the row that finishes the work records the
  tail of it and its suspended predecessors record the rest.
- **Start efficiency**: loading context is pure overhead, so no attempt
  starts unless the window can fund at least as much work as the context
  it must load (`MIN_START_EFFICIENCY` = 0.5; never less than 25k of
  work). A resume re-sends its whole transcript (measured: re-cache cost
  equals the context at suspension), so it reserves that context plus
  the larger of what is left of a typical job and that minimum — a
  100k-token transcript reserves at least 200k. With a flat 25k on top it
  would reserve 125k, and an attempt granted 125k spends 80% of it
  re-sending the transcript and 20% working. A cold start reserves the
  larger of its history estimate and 40k (a 15k system-prompt floor plus
  25k of work), which also stops a collapsed estimate from starting jobs
  that die on their second call. A larger reservation does not shrink
  the granted cap — the cap is computed without the job's own
  reservation — it makes the gate refuse until the window can fund both.
- A granted job's token cap is the ramp's remaining headroom, verbatim —
  there is no separate configured per-kind cap. Workers are metered
  **externally**: the harness tails the worker's live session JSONL and
  SIGTERMs the process group at that cap, never depending on the worker
  cooperating with its own limit. A grant with no headroom figure carries
  no token bound, and `maxWallS` alone stops it.
- Stale or missing usage data → conservative denial, never an optimistic
  guess.
- The budget gate only reads usage data, and data recorded before the
  machine went offline still grants. The network gate in § Scheduling
  loop is what keeps a worker from starting without its provider.

### PR follow-up loop

`sync_prs` runs first in every cycle (free — no worker) and refreshes every
open PR's state. Suppression is **state-based, not time-based**: an engage
reply that changes nothing (no commits pushed) records a fingerprint of
the *current static problem* (review decision, merge conflict, and
*which* checks are failing); `sync_prs` won't re-flag that exact static
situation again no matter how long the daemon then runs unattended — only
a genuine change (a different check starts failing, the review state
changes, or a human pushes new code, tracked by head commit SHA so a
same-looking situation after a real push is never mistaken for
"unchanged") clears the suppression. This replaced an earlier flat
time-based backoff, which would have kept re-poking an unfixable problem
every N minutes forever on a long-running unattended daemon.

## Web UI

Served at `http://localhost:8377` (configurable). Left-nav pages:

| Page | Shows |
|---|---|
| **Status** | Budget bars (used + available, per window), Run/Pause and Overdrive controls, what the scheduler is doing right now / why it isn't, and the recent event log |
| **Inbox** | New findings awaiting triage, with per-type filters |
| **Kanban / Pipeline** | Findings in flight, a separate Needs you section with held work (why it is held, the report, and its Continue/Restart/... controls), suppressed (rejected/wontfix), and informational (note) findings |
| **All Findings** | Every finding, filterable by repo/type/status/severity, with full detail (jobs, PR state, timeline) on expand, and the actions its status allows (verdicts on new/note/closed, releases of held work, Recheck on new, Unqueue on queued, budget override while something will still pick it) |
| **Repos** | Add/remove/pause repos; queue a full re-hunt (the next hunt reviews the repo's complete history instead of only new commits — the same scope as the periodic `hunt.rehuntDays` re-hunt — and the request stays pending until such a hunt finishes cleanly); per-repo notes (free-text context injected into every prompt for that repo — coding conventions, known false positives, anything worth a hunter remembering across runs) |
| **Stats** | Aggregate totals by kind and by finding type |
| **Log** | Full job and event history |

Inbox, Kanban, and All Findings store their filters and sort order in the
URL, so copied links, reloads, and browser Back/Forward restore the view:

```text
#findings?type=bug&severity=high&confidence=80&sort=newest
#inbox?type=bug&type=test_gap
#findings:123?status=pr_open
```

Parameters: `repo` (repository name), `type`, `class`, `severity`, `status`
(All Findings only), `confidence` (0–100, in steps of 5), and `sort`
(`severity`, `newest`, `updated`, `oldest`). Repeat a selection parameter
for multiple values; omit it for all, or use an empty value (`type=`) for
none. Explicit choices remain selected even while their options are absent
from the current data. Selecting the last currently offered option keeps a
deep-linked list explicit; future options are not silently admitted. Use
the All control to return to an unrestricted selection. Defaults are omitted
when controls update the URL.

Card numbers (`F#123`) link to the finding on All Findings
(`#findings:123`), carrying the current page's filters and sort, and can be
copied or opened in another tab. The linked card is placed at the top of the
list and outlined. If a carried filter hides it (its status or confidence
changed since the link was made), only that filter is dropped; an unknown
number shows a "not found" notice. Changing filters or sort keeps the focus
while the card stays visible; a filter that hides it drops the focus.

Back, Forward and reload return each page to where it was scrolled. Long
card lists come back to the card that was at the top of the view, not to a
pixel offset. A new navigation, including a filter or sort change, starts
at the top, or at the focused finding; and a saved position wins over a
link's finding, so Back to a `#findings:123` view returns to where you had
scrolled, not to F#123.

## Users

The dashboard and every API route need a login. Accounts are local
(argon2id password hashes in `hunter.db`) and created by the operator;
there is no sign-up page:

```sh
cd hunter-rs
target/release/hunter --root ../hunter user add alice     # asks for the password twice
target/release/hunter --root ../hunter user passwd alice  # new password; logs alice out everywhere
target/release/hunter --root ../hunter user disable alice # disables the account; logs it out
```

The password is read from the terminal without echo, or from the first line
of stdin when that is not a terminal. Passwords need at least 12
characters. The commands work next to a running daemon.

A login lasts 30 days, in an `HttpOnly`, `SameSite=Strict` cookie; only a
hash of its token is stored. Every logged-in user can do everything — there
is no per-user data yet — but what a person does through the API (verdicts,
notes, overrides, repo changes) is recorded as theirs in the event log.

## API

JSON routes — the UI's only client, but usable directly. Served by axum
(`router` in hunter-rs/src/server.rs). Every route except `/api/login`
answers `401 {"error": "login required"}` without a live session cookie.

| Method | Path | Purpose |
|---|---|---|
| POST | `/api/login` | Log in (`{"username", "password"}`); sets the session cookie |
| POST | `/api/logout` | End this session |
| GET | `/api/me` | The logged-in account |
| GET | `/api/summary` | Budget windows, counts, activity status, "what's next" preview |
| GET | `/api/findings` | List findings across all types (`status`, `repo`, `severity`, `type` query params) |
| GET | `/api/finding` | One finding's full job history + PR state |
| GET | `/api/jobs` | Recent jobs (last 50) |
| GET | `/api/repos` | List repos |
| GET | `/api/repo/notes` | One repo's notes |
| GET | `/api/events` | Recent event log |
| GET | `/api/stats` | Aggregate stats |
| POST | `/api/verdict` | Set a finding's triage status (queue for fix, wontfix, reject, note) |
| POST | `/api/cycle` | Trigger one scheduler cycle immediately |
| POST | `/api/scheduler` | Pause or resume automatic scheduler cycles (`{"paused": boolean}`); a running job is allowed to finish |
| POST | `/api/overdrive` | Enable or disable global prioritized budget mode (`{"enabled": boolean}`); provider hard stops remain enforced |
| POST | `/api/recheck` | Queue a finding for recheck |
| POST | `/api/unqueue` | Pull a finding back out of the fix queue |
| POST | `/api/override` | Set/clear a finding's budget override (`once` \| `exempt`) |
| POST | `/api/repo` | Update a repo (pause/resume, etc.) |
| POST | `/api/repos` | Add a repo |
| POST | `/api/repo/delete` | Remove a repo |
| POST | `/api/repo/rehunt` | Queue a full re-hunt of a repo |
| POST | `/api/repo/notes` | Append a note to a repo |

## Architecture

```
hunter-rs/src/
├── main.rs         CLI: `--root`, `--port`
├── daemon.rs       UI server + scheduler loop + usage prober, lockfile
├── config.rs       config.json loading
├── domain.rs       status/kind enums
├── types.rs        API response + row types (the UI contract)
├── store.rs        SQLite access layer; migrations in hunter-rs/migrations/
├── backend.rs      Backend protocol (decide / run / status)
├── backends/omp_scavenge/
│                   budget policy (§ Budget policy), `omp -p` harness that
│                   meters + kills at cap
├── forge.rs        GitHub/GitLab abstraction (gh/glab CLI wrappers)
├── ingest.rs       validates + dedupes a worker's findings.json into the store
├── playbooks.rs    renders playbooks/*.md templates into worker prompts
├── dep_scan.rs     Renovate-based dependency scan (zero tokens)
├── scheduler.rs    one cycle = one job; run_hunt/run_fix/run_engage/
│                   run_harvest/run_recheck/pick_next/run_cycle
├── server.rs       axum JSON API + serves ui/
└── util.rs         shared subprocess helpers

hunter/
├── playbooks/      worker prompt templates (one per job kind, see table above)
├── ui/             generated Vite bundle (ignored; built by `just build` in hunter-rs/)
├── ui-svelte/      tracked Svelte source
└── config.json     runtime config (§ Configuration)
```

Each analysis/action kind follows the same shape: build a prompt from a
`playbooks/*.md` template, run it through the backend under a
budget-decided cap, then either ingest structured output (`findings.json`,
`FOLLOW-UPS.json`) or interpret marker files the worker wrote
(`NOT-A-BUG.md`, `PR-DESCRIPTION.md`, `WITHDRAW.md`, ...).

## Data model

SQLite (`data/hunter.db`, WAL mode). Tables (see `hunter-rs/migrations/`
for the schema — most columns carry comments explaining *why*, not just *what*):

- **`repos`** — registered repositories, hunt watermarks, enabled/paused.
- **`findings`** — one row per finding across all five types (type
  discriminator column), status, common fields, plus per-type nullable
  columns (bug\_class, dep\_update ecosystem/package/versions, test\_gap
  missing\_tests, refactor smell\_type, modernization\_class), and retry
  streak tracking (fix/recheck) for the give-up mechanism.
- **`jobs`** — one row per worker invocation: kind, cap/actual tokens,
  exit/kill reason, timing, and `resumed_from` (the suspended attempt this
  one continues). States are `running | done | failed | killed | suspended |
  denied`; `suspended` is a pause with a session file to
  continue, everything else killed is terminal. The audit trail for "what
  did hunter actually spend, and on what."
- **`pr_state`** — one row per finding with an open/merged PR: forge state,
  attention fingerprint + suppression marker (state-based, see § PR
  follow-up loop), harvest tracking.
- **`window_log`** — mirror of budget observations at decision time.
- **`calibration_samples`** — empirical tokens-spent → used-fraction-moved
  correlation, informational only (estimates real remaining capacity;
  never gates a decision).
- **`events`** — append-only log backing the Log/Status pages.
- **`scheduler_state`** — single-row snapshot of the daemon's own current
  reasoning, purely informational.

## Configuration

Copy the provider-matched template to the ignored `config.json`:

```sh
# Anthropic
cp config.anthropic.example.json config.json

# OpenAI Codex
cp config.openai-codex.example.json config.json

# GitHub Copilot
cp config.github-copilot.example.json config.json
```

The templates share operational settings but pair the provider with compatible
worker models. Do not commit `config.json`; it selects the credentials and
quota source for one installation.

- `--root` may be relative. The loader resolves it to an absolute path
  before resolving `workRoot`, `dbPath`, playbooks, and UI paths, so Git's
  clone-relative working directory cannot relocate worker worktrees.
- `hunt`/`fix` settings apply to their whole job family (hunt also covers
  test\_gap/dep\_update/refactor/modernization/recheck; fix also covers
  engage/harvest/apply\_\*).
- `budget.deny5hAbove` / `staleAfterS` — see § Budget policy.
- `models` — fuzzy names, anything omp's `--model` flag accepts. `default`
  applies to all workers; `smol` is for lightweight helper tasks; `hunt`/
  `fix` override per job family; `null` inherits `default`.
- `backend.llmProvider` selects the OMP quota source: `anthropic` (default),
  `openai-codex`, or `github-copilot`. It must match the provider selected
  by `models`. The Copilot template uses provider-qualified model names
  to avoid fuzzy matching a model from another provider. Authenticate
  with `omp login github-copilot`; inspect quota with
  `omp usage --provider github-copilot`.
- Copilot meters premium **requests**, while hunter's worker watchdog
  meters **tokens**. Monthly token capacity is an estimate from completed
  calendar-month spend; without history it uses the existing long-window
  fallback (67.2M tokens). This is not an exact request-to-token conversion
  and cannot guarantee that a job won't exhaust its provider quota.
- `renovate.githubToken` — optional token for the dependency scan's
  github.com lookups (Actions, GitHub tags/releases). Only GitHub-hosted
  repos get a token: this one if set, else the `gh auth token --hostname
  github.com` login. Without either, those deps are silently skipped.
- `backend.providerHosts` — the hosts a worker needs, probed by the
  network gate (§ Scheduling loop) in place of the provider's own, each
  `host` or `host:port` (443 when omitted), e.g. `["llm-gw.corp:8443"]`
  for a gateway. Default: the provider's API host.
- `network.gate` — `false` turns the network gate off, for a machine that
  reaches its forge and provider only through a proxy, where a direct
  connection never succeeds. Default `true`.
- `feedback.bots` — review bots whose PR comments engage and harvest act on,
  e.g. `["coderabbitai"]` (either `name` or `name[bot]`). Besides these,
  only people who can push to the repo count (GitHub: write access or above;
  GitLab: Developer or above, looked up per author and cached for 15
  minutes). Everyone else's comments and reviews are dropped before the
  scheduler or a worker sees them: on a public repo anyone can comment on
  hunter's PRs, and engage commits and pushes in response. Bot reviews
  reach the worker in their own section, to verify rather than obey. On
  GitHub, where `gh` reports an app by its bare name (`greptile-apps`) and a
  person may hold that same login, each comment or review by a listed name
  counts as the bot's only if GitHub reports its author as a `Bot`
  (GraphQL `__typename`, asked per item); the person's are screened like
  anyone else's. Default `[]`: no bot is listened to.
- `serve` controls who can reach the dashboard (Rust daemon only; the Python
  rollback always serves loopback). Defaults keep it local:
  - `serve.host` — address to bind, default `127.0.0.1`. `0.0.0.0` listens
    on every IPv4 interface.
  - `serve.allowedHosts` — names accepted in the HTTP `Host` header, beyond
    the loopback names that are always accepted. List the names clients use,
    e.g. `["hunter-box", "192.168.1.20"]`. `["*"]` accepts any name and turns
    off the DNS-rebinding guard.

  Every API route needs a login (see § Users), but the session cookie
  travels in clear text over plain HTTP: put TLS in front (e.g. Caddy)
  before widening `serve.host` beyond a network you trust. Widening
  `serve.host` makes the dashboard reachable; widening `serve.allowedHosts`
  decides which names it answers to.

## Development

Rust gates live in `hunter-rs/justfile` (`just lint` and `just test` run fmt,
clippy, `check-sql.sh` and nextest — the same commands CI runs). The
frontend lives in `ui-svelte/`; Vite writes the bundle into the generated
`ui/` (`ui-svelte/vite.config.ts:15`), which `just build` in `hunter-rs/`
does alongside the release binary. Frontend checks: `npx eslint src/`,
`npm run check`, `npx vitest run` in `ui-svelte/`.

Conventions: strict TypeScript on the frontend, with the API types generated
from `hunter-rs/src/types.rs` by ts-rs into `ui-svelte/src/lib/generated/`
(`just bindings` in `hunter-rs/`; CI fails when they are stale) and
hand-rolled structural validation at the network boundary
(`ui-svelte/src/lib/validate.ts`, which checks exactly what the components
dereference unconditionally), so a shape drift between daemon and UI fails
loudly instead of silently rendering garbage.

## Running as a service

The systemd user unit is checked in at `hunter.service`. Its `ExecStart`
runs the Rust binary directly — `<project-root>/hunter-rs/target/release/hunter
--root <project-root>/hunter`, with `WorkingDirectory=<project-root>/hunter-rs`
(hunter.service:6-7) — so `<project-root>` is the repository root, not this
directory. The daemon never builds the UI: it reads `hunter/ui/` from disk
at request time, so a binary built without the bundle serves a working API
behind a 404 page. `just build` builds both halves.

```sh
(cd hunter/ui-svelte && npm ci)                            # frontend deps; `just build` does not install them
cd hunter-rs && just build && cd ..                        # release binary + ui/ bundle
sed -i "s|<project-root>|$(pwd)|g" hunter/hunter.service   # repo root, not hunter/; `g`: ExecStart has two
cp hunter/hunter.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now hunter.service
loginctl enable-linger $USER   # keep it running after you log out
```

After a rebuild, `systemctl --user restart hunter.service`; `just build`
deliberately does not reach into the service manager itself.

Upgrading across migration 022 (`verdict_anchors`): suppressions that
existed before it have no anchor and stay unconditional until a new verdict
replaces them. To anchor them, insert `verdict_anchors` rows only after the
daemon has started once on the new binary, which runs the migrations; a
table created by hand beforehand makes the migration fail at startup.

It idles at zero token cost between cycles and wakes on a smart-sleep
policy: right after a job with more queued work → drain immediately;
budget denied → short retry if the ramp is actively rising, otherwise sleep
to the window reset; genuinely idle → 15 minutes. PR-comment polling is
decoupled from the token-budget backoff loop, so a long budget-driven sleep
never delays noticing new PR feedback. Every wake re-checks the budget gate
before spending anything, and starts no job while the filesystem holding
`workRoot` has less than 1 GiB free (retried every 5 minutes), or while
the picked work's forge or provider is unreachable (retried every 3
minutes).

