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

Hunter clones, fetches and pushes fix branches over that URL exactly as
given, so it must be one you can push over: an `https://` URL through a
git credential helper (`gh auth setup-git` installs `gh`'s), or an ssh
`git@host:owner/repo.git` URL through your SSH keys. Plain `http://` is
refused: a push over it would send your credentials unencrypted.

## How it works

### Finding types

| Type | What it looks for | Prompt |
|---|---|---|
| `bug` | Latent bugs — boundary, error-path, race, contract-drift, leak, logic | `playbooks/hunt.md` |
| `test_gap` | Missing test coverage for real code paths; untested code that already misbehaves is filed as a `bug` instead | `playbooks/test_gap.md` |
| `dep_update` | Outdated dependencies, one finding per Renovate branch (see below) | `playbooks/dep_update.md` |
| `refactor` | Conservative, safe, mechanical refactoring opportunities | `playbooks/refactor.md` |
| `modernization` | SOTA-drift — deprecated deps, language-feature gaps, format/protocol shifts, CI/CD gaps, platform EOL | `playbooks/modernization.md` |
| `standards` | Structural deviations from `CODING_STANDARDS.md` | `playbooks/standards.md` |
| `simplification` | Complexity the code no longer needs — generality with one user, unreachable variants, over-powered data structures, accreted special cases, duplicated state, scaffolding of removed features, custom constructs where the conventional one would do. Never by loosening types | `playbooks/simplification.md` |

All seven share one `findings` table (see [Data model](#data-model)) and one
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
- **Pre-PR review** — before a finished fix ships, still inside its fix
  job, a fresh reviewer session (`playbooks/review.md`) told to distrust
  the change reads the whole branch diff and writes its issues outside
  the tree. Every issue it raises (blocking or not; `blocking` ranks
  them) goes back to the fixer's own session
  (`playbooks/review_followup.md`), which fixes each one with commits or
  rebuts it with evidence in a `## Review` section of
  `PR-DESCRIPTION.md` (or declines or writes `BLOCKED.md` after all);
  then the whole branch is reviewed again, until a review raises no
  issue or `review.maxRounds` reviews have run. The reviewer reads the
  `## Review` section and does not raise a rebutted issue again without
  new code evidence. Defects outside the change's scope are reported as
  `follow-up`s: never answered and never keeping the loop going, they are
  collected across rounds and ship in a "## Follow-ups from review"
  section of the PR body. When issues are left after the last round, any
  of them holds the fix for you like a `BLOCKED.md`, with the remaining
  issues as the hold's report: a review still finding something after
  `review.maxRounds` rounds is not converging. `continue` resumes the
  fixer on them and the review runs again. When the grant is spent before
  the review is through (before a round, by a reviewer killed at its cap,
  or before the fixer can answer a round), the job is suspended, and its
  resume, under a fresh grant, answers the issues left or has the
  cut-off round again, without first resuming the finished fixer: a
  review cut short never ships as one. When the fixer's answer fails
  without doing work while the forge is reachable (offline, the attempt
  is kept and retried), a blocking issue holds the fix the same way and
  non-blocking ones ship in an "Unresolved review issues" section of the
  PR body; a review that fails
  (a reviewer that ended early or left
  no valid output without writing an issue) ships the fix with a note
  saying so. The change is its commits: before every round the fixer's
  uncommitted changes to tracked files are discarded and its untracked
  files deleted, in every checked-out submodule as well, which is put
  back to the commit the tree records (ignored files, such as build
  caches, and `PR-DESCRIPTION.md`, untracked or committed and then
  updated, stay), and the round's event lists what went; what cannot be
  removed stays, named in the event, and the fix is reviewed with it. Whatever a reviewer then changes in the tree is undone (the branch
  the reviewer was shown, the fix branch or one of the fixer's own,
  checked out again at the reviewed commit, untracked files
  deleted, submodules cleaned the same way). The verdict is about the
  commits: a reviewer that left them alone, whatever it left uncommitted
  (a test run's artifact, a lock file its build rewrote), changed nothing
  it reviewed: its changes are reverted, the round's event names them,
  and its verdict stands. One that committed, moved the branch or touched
  `PR-DESCRIPTION.md` has issues it had written still count. When its changes cannot be undone,
  the tree is put back to the commit the reviewer was shown before any
  later attempt runs a worker in it, however that round ended (its issues
  held the fix, a host went unreachable, or the publish of that commit
  failed); while that fails (a hold's report, and each failure, names
  what is still in the way) no worker runs, and the attempt fails as
  `reviewed tree not restored`, three times in a row holding the fix
  `stuck`. Until a fixer has run on the review's issues, any `continue`
  resumes it on them, whichever hold it released. A fixer
  that did not finish ships nothing while the review is on. Every run is
  metered into the one fix job, under its one budget grant. Details:
  `hunter-rs/WORK-MODEL.md` §5.7. Before the push, a multi-commit fix is
  curated into a few logical commits of the same tree (`review.curate`); a
  curation whose changes cannot be undone is put back the same way as
  a reviewer's before any later attempt runs a worker. A curation cut
  short (a grant that runs out before or during it, a restart, a rewrite
  that had to be put back) is owed: the next attempt runs no fixer and
  curates before it ships, as for any other part of the fix.
- **Held work** is not a status. When a worker writes `BLOCKED.md`
  (a verification prerequisite, dependency, or human decision is
  missing; every finding kind's playbook says when: a recheck that cannot
  verify, an engage whose feedback needs a maintainer's decision, a
  harvest that cannot classify the closure), or the same failure repeats
  three times in a row, the work is *held* for you. For a fix: the
  finding stays `queued`, the scheduler skips it, and
  the attempt is kept as its checkpoint (committed work, uncommitted
  reports, transcript) when a worker ran in it. It is NOT suppressed.
  Kanban's **Held** column lists it with the report or what kept
  failing, and the ways out: **Continue** resumes the checkpoint with a
  fresh give-up budget (a prerequisite hold asks the worker to re-evaluate
  it, a failure hold to finish the work), **Restart** starts over,
  **Reject**/**Wontfix** decide the finding. A recheck whose worker fails
  the same way three times is held the same way and stays `rechecking`,
  where it used to go back to `new` looking like a finding nobody had
  rechecked. Whatever the kind, **Continue** tells the resumed worker why
  it was held, with the report or the failure as context.
- **`pr_open`** — tracked via `sync_prs` (free — `gh pr view`, no worker)
  every cycle: merged → `merged`; closed unmerged → `closed`; new
  comments/reviews, `CHANGES_REQUESTED`, merge conflicts, or failing checks
  flag it `needs_attention` for an **engage** worker, which may also
  withdraw the PR (→ `closed`); the same worker session then carries on
  into the closed-PR harvest below when the window has room, else the
  harvest runs cold later.
  An engage that keeps failing the same way (the worker never finishes,
  the push or the reply fails, the forge refuses the withdrawal) is held
  for you after three in a row; new PR activity does not release it, but
  the card says there is some. **Dismiss** marks the PR's current
  attention handled, as an engage that found nothing to do would.
- **`closed`** — the PR closed without merging. Deliberately NOT a
  rejection: of the first 16 closed PRs, 15 were engage withdrawals (14 of
  them superseded or obsolete) and one was closed by a human, so a closure
  on its own says little about whether the finding was wrong. A one-time
  **closed-PR harvest** reads the PR's diff, discussion and the current
  code, classifies the closure (`CLOSE-REASON.json`) and files what it
  left open (`FOLLOW-UPS.json`): superseded / duplicate / obsolete →
  `superseded` (valid, not suppressed), wrong → `rejected`, unwanted →
  `wontfix`, abandoned → `new`. A review that cannot classify it counts
  toward the harvest's failure streak; at the limit the harvest is held
  for you and the finding stays `closed` until you continue or restart
  the harvest, or classify the closure by hand.
- **`merged`** → queued for a one-time **harvest** pass reviewing the PR's
  complete lifetime (not just its open-time snapshot) for genuine deferred
  follow-up work, filed as new findings via `FOLLOW-UPS.json`. A harvest
  that keeps failing (a PR that will not load, a clone that will not
  fetch, a worker that never finishes) is held for you like a fix; for a
  merged PR, **Mark harvested** ends it without a review. A held harvest
  files its follow-ups only when you end it that way (or with **Classify**
  on a closed PR); **Continue** or **Restart** reviews the PR again, and
  that review files them. A paused harvest continues without asking the
  forge for anything, so a network that is not back yet costs it nothing.
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
"what's next" preview calls the same function, so its order can never
drift from the cycle's). The preview does not see what only the cycle
learns as it runs: it does not probe the network gate below, so while a
repo's work is passed over for its unreachable forge the preview still
names that work, and the `deny` event the cycle leaves for it says why it
did not run. Nor does it hold back an engage waiting for the PR sync.
The order:

1. Any finding with an active **budget override** (any category) — human
   escape hatch, jumps the whole queue.
2. **Engage** the flagged PR with the oldest-outstanding attention reason.
3. **Review** a requested PR (§ PR reviews) whose next round is due: the
   oldest request first. A human is waiting on it like on a flagged PR.
4. **Harvest** the oldest merged or closed PR still pending its review
   (a closed PR qualifies only while its finding is `closed`, which no
   verdict can set; a finding a human moved on after the closure, even to
   `rejected`, is left alone. Closures from before the closed-PR harvest,
   which left their finding `rejected`, were moved to `closed` once by
   migration 016).
5. **Recheck** the oldest finding stuck in `rechecking`.
6. **Fix** the oldest queued finding.
7. **Resume** the newest suspended attempt that can still be continued.
   A worker killed for running out of window headroom keeps its session
   and comes back as `suspended` rather than `killed`, and the next cycle
   hands omp that exact session instead of starting cold. So does one
   that died after doing metered work — a provider error omp's retries
   did not absorb, a connection lost across a laptop suspend, or the
   daemon going down mid-job (found at the next startup), and one killed
   for running past its wall-clock limit after doing metered work. Never
   an attempt that failed before spending a token (a misconfigured model
   or an exhausted account fails the same way again). A runaway is bounded
   by the give-up ceiling, not by refusing to continue it: a chain that
   keeps suspending is given up, and its finding's work counts that as a
   failure.
   Ranked here because a suspension has already been paid for — it
   outranks *starting* new background work, never a human waiting on a
   PR. Restarting costs a flat ~37k-token session floor and then redoes
   the work; continuing costs re-caching the context the worker was
   already carrying.
7. Otherwise, **hunt/test_gap/dep_update/refactor** — whichever is most
   overdue for the least-recently-scanned enabled repo (a never-cloned repo
   always hunts first). **Modernization**, **standards** and
   **simplification** join this rotation too, but each only once its own
   `modernization.intervalDays` / `standards.intervalDays` /
   `simplification.intervalDays` (config, default 30) has passed since it
   last ran for that repo — periodic strategic checks, not tight-loop scans
   competing for every cycle. A repo with a **full re-hunt** queued from the Repos page
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
is sent. If the picked work's forge is unreachable while the provider
answers, the cycle passes over that repo's work, and that of every other
repo on the same forge, to the next candidate, so one repo on a forge out
of reach (a GitLab behind a VPN, say) does not stop the others. If no
candidate is left, or the provider is unreachable (alone, or with the
forge, as on a machine with no network), the cycle is denied with a
`deny` event naming the first host found unreachable and retried in 3
minutes: nothing runs, nothing counts toward a failure streak or a
rotation turn, and no job row, claim or checkpoint is touched. Work passed
over is not touched either, but each cycle that passes it over leaves a
`deny` event naming it, its repo and the host it waits for
(`offline: <host> unreachable; passed over <kind> on <repo>`), so a repo
whose forge stays out of reach does not drop out of sight while other work
runs. A cycle that passed work over and then ends denied anyway (say, the
next candidate's budget) still looks again within 3 minutes, so the work
passed over runs soon after its forge is back. The daemon runs on a laptop
that travels
and sleeps, so being offline is a normal state, not a failure of the work
it would have started. The Status page shows a denied cycle as
**offline**, naming the host, until a cycle gets past the gate.

The network can also go while work runs. So before a finding's failure is
counted, the same hosts are probed again: a forge call that failed before
the work started (a harvest's fetch, PR view or diff, an engage's target
or PR view), a backend that could not start the worker, a worker that
failed without doing any metered work, and publishing a finished
worker's work (a fix's push or PR, an engage's push, reply or
withdrawal). A finished attempt that could not publish is kept as the
checkpoint, so the next cycle publishes it rather than redoing it. If a host is unreachable then, the
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
killed, its backend or tree fails, or it finishes without output or with
any invalid entry — is a failure of that scan: its `last_<kind>_at` stays
where it was, so it is retried as soon as the rotation reaches it, and the
third identical failure in a row holds it for you. A failed hunt leaves its
watermark where it was, so the retry reviews the same commits. A failure
that breaks the whole repo (the clone or fetch fails, the clone's origin is
another repository, `origin/<branch>` does not resolve) is counted once,
on the repo's **sync**; while that is held, nothing of the repo runs. A
held scan is skipped and the turn passes on. A suspended attempt is not a
failure; tier 6 continues it, and a chain abandoned at the give-up ceiling
counts as one failure.

A repeatedly-failing item (same failure reason, consecutive attempts) is
held for you after a bounded streak rather than looping forever — this
applies uniformly to every finding kind and every repo scan. A resume
chain has the same kind of ceiling, on either of two counts: once it has
made four useful-work attempts, or once — after at least one resume —
everything it spent *besides its single largest attempt* is past three
times what that kind of job typically costs. Whichever fires, the
work item drops the suspension as its checkpoint (the job row stays as it
ended) instead of continuing it again. A chain
that has never been resumed is never retired: one attempt's spend is by
definition at least its own cap, and the largest attempt is discounted
because one enormous attempt says the job is big, not that the chain is
stuck. For every kind, finding work and repo scans alike, a given-up
chain is one failed attempt in that work's streak, so three given-up
chains in a row hold the work like three identical failures; otherwise a
worker that suspends on every attempt would have its work started fresh
forever. The ceiling bounds what
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
  item drops its pointer, finding work and repo scans alike; the old
  attempt's row stays as it ended.
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
  it (the work item drops its pointer; the row stays as it ended) and
  moves on to the next candidate the same cycle.
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

New comments are tracked by a per-PR engagement watermark: a screened
comment or review newer than it flags `new_comments`. A finished engage
moves the watermark only to the newest feedback its worker was shown —
read once, when the chain starts, and carried through every resume
(`jobs.seen_activity_at`), since a resumed worker is only told to carry
on — so anything posted while the worker ran or sat suspended is still
flagged at the next sync. Hunter may post under the operator's own
login, so its own comments are recognised by the hidden
`<!-- posted by hunter -->` line `comment_pr` appends, not by author:
from someone who can push, such a comment is never new activity, and
engage prompts show it in the thread as hunter's own earlier reply.

### PR reviews

Hunter can also review pull requests it did not open (future.md §8.8).
A review is requested by hand on the Reviews page. Hunter's own PRs are
refused: they are engaged, not reviewed. A finding's PR is hunter's from
the moment it is opened (its `pr_url`, not only once the PR sync recorded
its number), and a review requested before hunter opened its PR ends as
*hunter's own PR* at its next round or PR sync. Reviewing runs the PR's code with
the daemon's credentials, and that code is the repository's the head lives
in: a head in the repo itself was pushed by somebody who can push, a fork's
by the fork's owner and whoever they let push there. So the author and, for
a head in a fork, the fork's owner (GitHub's `headRepositoryOwner`, a
GitLab source project's namespace) must each be able to push to the repo, or
the request needs the requester's vouch for each who cannot, even one whose
login is a `reviewBots` entry: a login is only a name, and a human may hold
a bot's. A maintainer opening a PR from a stranger's fork is no vouch for
the stranger; a fork owner who is the author is decided about as the author,
and one the forge does not name (the fork is gone) needs a vouch. A vouched
fork owner stands in for everyone who can push to their fork. The review
records the fork's owner it decided about (`pr_reviews.decided_head_owner`),
and every read of the PR records the owner it finds (`head_owner`): once a
fork changes hands (its owner transferred it), the review runs no round —
the round that finds it skips, and none is due — and the Reviews page says
why until a request decides about the new owner. Hunter's forge
login must be able to push to a repo whose PRs it reviews, as to every
repo it tracks: GitHub tells who can push only to those who can, every
trust check here rests on that, and resolving its threads needs it too
(GitLab, which tells any member, is asked about hunter's own login
first); a request where hunter's login cannot push fails saying so.

Each round fetches the PR's head (`refs/pull/N/head` or
`refs/merge-requests/N/head`) and runs a reviewer (`playbooks/review.md`;
it may build and run targeted tests, but must leave the tree as it found
it) in a tree detached at that head. The first round reviews from the
merge-base with the PR's base branch. Later rounds review only from the
head the last round that finished reviewed, as long as the PR still descends
from it and the merge-base has not moved past it (after a force-push, or
once the base branch was merged into the PR, from the merge-base again:
the round would otherwise review the merged-in upstream commits). Every
round is shown, and judges, every issue reported since the last round that reviewed
from the merge-base, plus every older issue that is still open or whose
thread hunter has not resolved yet: a
force-push does not settle what it replaced. A discarded round's issues
are never shown: they were never the review's. With **Watch** on, every new head is
reviewed once it has stayed unchanged for 10 minutes (a round that finds
the PR moved again since it was picked skips, and the newest head waits
out its own 10 minutes), and replies alone bring a round too (below);
with it off, only the round a request asks for runs. Every request asks
for one at the head the PR has now: requesting a review that already
follows the open PR again reads the PR, and the threads of its posted
issues (one forge call, as a round reads them; a failed read fails the
request as a failed read of the PR does), and reviews that head, held or
not, unless a round already reviewed it and no news in its threads awaits
judgment (below). A request waits (`pr_reviews.round_requested_at`, with
the head it read) until a round that read the PR after it answers it: a
finished one, whatever head it then reviewed; or one that found the PR
at the reviewed head and, reading the threads, nothing awaiting
judgment there (the news it was asked for deleted since, or the PR
force-pushed back), which runs no reviewer and records no round. A
request made while a round runs waits, whatever head it read, even the
one the round reviews: it may be for a reply the round never read. Only
a round that fails or stops partway, and is tried again, leaves the
request it took waiting, and a head the PR sync reads while a round
runs is no request: an unwatched review runs only the rounds asked for,
and a watched one's new head waits out its 10 minutes. A round due by the
clock alone (a head settling, a reply going quiet) starts when it comes
due: the job lane's idle sleep ends then, and a PR sync that brings a
round forward wakes the lane to sleep until it instead. A
paused (disabled) repo's reviews still run:
pausing stops scans, not work a human asked for; a repo no job has cloned
yet is cloned first. A reviewer stopped by its token cap or the wall
clock keeps the issues and verdicts it already wrote (the playbook has it
rewrite each file after every entry), but did not review the whole range:
its round is *partial*, the head stays unreviewed, and the next round
reviews the same commits again, shown what the stopped one wrote. One
stopped before writing either failed its round, and so did one that
changed its worktree (a file a test left behind counts) and wrote neither:
its silence is no proof the PR is clean. An issue or a verdict a reviewer
wrote stands whether it changed the tree or not: the tree is discarded,
and what it wrote is about the PR's commits. The PR sync refreshes
each open review's head, its fork's owner, title and state. A merged PR ends its review for
good; a closed one ends it until the PR is reopened: requesting the
review again then reads the PR and, open again, reviews it again at the
head it has now, watched or not (a round that already reviewed that head
leaves nothing to review), with the trust decision the first request made
(while the PR is still closed the request is refused). A review is bound to the
repo URL it was requested under (`pr_reviews.repo_url`): that is where the
request decided whether the author can push, and which PR its number
names. Once the repo's URL is changed to another repository's (an edit
of its form alone — `.git`, `http` to `https`, an `ssh` remote, the
host's case — names the same PRs, and moves nothing), the same number
may name another
PR, by an author nobody decided about, so the review does nothing — no
round, no read of the PR, no post — and the Reviews page says why until a
request binds it to the new URL, reading the PR there (refused unless it
is open) and asking the push check (or a vouch) afresh. That holds for a
review that ended too: merged or closed said so of the PR at the earlier
URL, so the rebound review is open (a review that ended as hunter's own
stays so: hunter's PRs are the repo's at any URL, and a request for one
is refused). That starts the
review over (its `binding`
goes up by one): every round so far was about the PR at the earlier URL,
so none of them counts again — the first round at the new URL reviews
the PR from its merge-base, is told none of their issues and reads none
of their threads, and a round that was running at the earlier URL when
the request came changes nothing. A failed or partial round counts
toward a hold at 3 in a row (a reviewer whose cap never lets it finish
the range is held, not run forever); a finished round, a round for
replies that judged some before its cap stopped it (progress, though
partial), a request, turning Watch on or **Retry** clears it. Until
then the next round waits out a backoff from the failure's end
(`pr_reviews.failed_at`): 5 minutes, doubled for each failure in a row
after the first, so a brief outage of the model backend does not spend
the rounds up to the hold back to back. Budget and offline denials
count nothing.

A round with issues is posted to the PR as one review, under the
operator's `gh`/`glab` login and therefore always a plain COMMENT review
(never an approval or a change request). Nothing goes out without a
decision: a round waits as *pending* until a user clicks **Post to PR**
(or **Discard**, which is final and takes back the round's verdicts: on
a watched review, the replies they judged bring a new round that judges
them again),
unless the review was requested with
**Post automatically** (`auto_post`, toggleable per review), in which case
a new round starts out approved. Toggling it touches only rounds recorded
afterwards. A round with nothing to post — no issues and no verdict that
changes what a thread on the PR says (below) — or a failed one has no
post state. A round of the review's earlier URL is never posted: a
request that binds the review to a new URL discards its rounds still to
post (one part of which already landed on the earlier PR stays as it
is), and only a round of the review's current binding is posted or
approved.
The admin lane's PR sync posts every approved round of an open review
right after refreshing the reviews; a round recorded approved wakes it at
once. Each issue whose line lies inside a
hunk of the PR's diff (the reviewer's `side`: `RIGHT` numbers the head,
`LEFT` the base, for removed code) becomes an inline comment; the rest —
no file or line, or outside every hunk — go into the review's body under
a header naming the round's `base..head`, and the body carries the
`<!-- posted by hunter -->` mark. The body stays within 65,536 characters,
GitHub's limit (GitLab takes 1,000,000 in a note), which a longer one
would fail on every attempt: its issues go in, in order, while they fit,
a line counts those left out, and the marks close it whatever was left
out; an issue's text past half that limit is cut, saying so, inline
too. GitHub gets one `pulls/N/reviews` call
at the round's head commit; GitLab gets one draft note per inline issue
(positioned on the MR's `diff_refs`) plus one for the body, each marked
and then published on its own, the body last (not `bulk_publish`, which
would also publish the operator's own drafts on the MR). GitLab runs a line that
begins with `/` and a command's name (`/merge`, `/approve`, ...) as a
quick action under the operator's login, and the reviewer's text quotes
the PR: every text hunter posts to GitLab, an MR's description included,
goes out with each such line escaped as `\/`, which Markdown shows as `/`
and GitLab runs nothing for. The inline comments' ids are
then read back from the PR, each found by its text as posted (GitHub: the
review's comments; GitLab: the MR's discussions opened by the login `glab`
acts as, since publishing gives a note a new id; anybody can copy hunter's
text, so a note somebody else wrote is never hunter's thread). Comments of
one text are told apart by the order the post created them in, which the
forge's ids follow: the newest comments of a text are the post's own, in
its order, and one an earlier round left with the same text is older. They
are recorded: a thread is found by its first
comment's id from then on. A read that fails leaves the review up and the
round approved with the error, and the next sync reads the ids again
without posting the review again; a comment the read does not find (it
lists every one) was deleted, and its thread is recorded *gone*. A round
counts as posted only once each of its inline comments has its id or is
gone. A round whose head the PR has since moved past is posted with
every issue in the body: its lines would land on other code; it names the
PR's current head, since after a force-push the round's own is no commit
of the PR and GitHub refuses it. So are the `LEFT` issues of a round
whose base is not the merge-base it resolved (`merge_base_sha`: they
number the head an earlier round reviewed, the PR's diff the merge-base;
an earlier round that ended at the merge-base makes no difference), and
every issue when the forge will not
serve the diff (GitHub refuses one past its size limits). A review that
went up but could not be recorded is recorded on the next sync rather than
posted again. A GitLab post that failed after publishing some of its
inline notes records those, and its retry posts only the rest (the
header still counts every issue). The drafts a GitLab post did not
publish — it failed, or the daemon stopped mid-post — are the operator
login's, and their next review by hand would publish them under their
name: a post marks its review before it creates any, and the sync deletes
every draft of the login on the MR that carries the mark (the operator's
own are left alone), right after the failure and on every sync until
that succeeds; until then the review posts nothing new, and each sync
its round cannot post for them counts as a failed post. A step the
forge took whose recording then fails (a review, those inline notes, a
thread's answer, resolve or unresolve) is held in memory and recorded on
the next sync instead of being taken again: that holds while the daemon
runs, so only a restart in that window says it twice. A failed
post keeps its error and is retried on the next sync; after 3 failures
the round falls back to pending with the error shown, for a user to
approve again or discard — but once any part of it may be on the PR (its
review was sent, an answer in a thread was sent, an inline note was
found: each call is marked before it is made, since it may land however
it fails) it is shown *partly posted* and can only be approved again:
what may be on the PR stays said, and the next post looks for it before
sending it again. A review send the forge refused outright (a 4xx answer:
none of it is on the PR) is unmarked again, so a round the forge refuses
for good can still be discarded. A post that failed while the forge host
was unreachable (network gate) counts nothing.

Later rounds also judge what earlier ones reported. Before taking the
clone's permit, a round reads the threads every one of hunter's inline
comments opened on the PR, settled issues' and older chains' too (GitHub: one GraphQL query of the PR's `reviewThreads`, matched
by the first comment's id; GitLab: the MR's `discussions`, matched by the
first note's id). A read lists every thread of the PR (one with more than
10,000, which only a server ignoring the page cursor reaches, is an error,
not a partial read), so a thread of hunter's it does not
hold was deleted: it is recorded gone, and no later read looks for it. A
round records each found thread's replies, screened to the authors a
review trusts: those who can push, configured `reviewBots`, hunter's own
login, and the PR's author (the request vouched for them). Everyone
else's replies are only counted. Each reply is dated by when it went out
on the forge (GitHub's `publishedAt`: a comment written in a pending
review goes out when the review is submitted, not when it was drafted),
and a comment still in a pending review is no reply yet: it is neither
kept nor counted until it is submitted.
A reply carrying the own-comment mark from hunter's login (the thread's
first comment's author) is shown as hunter's and flagged its own; anybody
else who types the mark, a pusher included, keeps their login and is a
claim to check. Only that flag makes a reply hunter's, so a person whose
login is `hunter` is answered like anybody, never taken for hunter
itself. If a thread hunter
resolved is found unresolved, a human reopened it and the issue is open
again — unless hunter itself set out to unresolve it: a post marks the
resolution it sets out to make before the forge call
(`pr_review_issues.resolving`), so a read that overlaps the call, or
comes after one that failed once the forge acted, takes the thread as
hunter left it, records hunter's step, and reopens nothing. A failed
read is logged and the round goes on without replies. The
reviewer is then shown every earlier issue of the chain, plus any older
one still open, reopened or rebutted (the author won that point, whatever
commits came since; an older chain's addressed one is about code the
new chain replaced, judged afresh) or whose thread hunter has not
resolved yet, with its id,
status and replies, and writes a second file of verdicts:
`addressed` (the code at HEAD fixes it), `rebuttal-accepted` (a reply
argued it away and the argument checks out against the code; accepted
only where somebody other than hunter replied) or `open`, each with a
one-sentence note. An issue it does not name stays as it was — silence
never resolves anything — and a failed round records no verdict.

Nothing records an issue's status: `pr_review_verdicts` keeps every
round's verdicts and notes, and the status is derived from them (the
`pr_review_issue_state` view): the newest verdict of a round not
discarded — `addressed`, `rebutted` or `open` — or *reopened* once a human
reopened a thread hunter had resolved, which voids every verdict given
over the thread as it stood before: a verdict counts only while no reopen
was recorded since its round read the issues, so one the PR sync records
while the reviewer runs voids that round's verdict on the issue. The
issue stays *reopened* until a round that read the reopen judges it,
whatever the verdict: `open` too answers the reopen. A verdict counts over
the replies it was shown as well, and only those: each keeps the newest
reply by somebody other than hunter its round saw (`seen_reply_at`), and a
newer one in a thread hunter has not resolved leaves the issue awaiting
judgment again, whatever the verdict said. Discarding a round therefore takes its verdicts back, and what they saw, with nothing
to undo. What hunter has said in each thread is recorded instead: its
standing answer (the verdict of its newest reply there), the newest reply
it answered, and whether it resolved the thread. A post makes every
thread of an issue its round judged or reported show that issue's newest
*released* verdict — of a round approved, posted, landed in part, or with
nothing to post — and does only what the thread lacks: a settled issue
gets a reply ("Addressed in `<head>`: <note>" or "Rebuttal accepted:
<note>", marked as hunter's) and its thread is resolved (GitHub: a REST
reply under the thread's first comment, `POST
…/pulls/:n/comments/:id/replies`, which publishes at once — GraphQL's
`addPullRequestReviewThreadReply` would drop it into the operator's
pending review, if they have one open, unseen while hunter counts it said;
with such a review open GitHub refuses the REST reply, and the post is
retried — then `resolveReviewThread`; GitLab: a
discussion note and `PUT …/discussions/:id?resolved=true`); an issue
found open again after hunter settled it (the fix was reverted) gets "Open
again at `<head>`: <note>" and its thread is unresolved; an open issue
whose thread, as the round was shown it, ended in somebody's reply (not
hunter's) gets "Still open: <note>" and its thread stays open, so a
rebuttal gets an answer either way. A round with such a verdict has
something to post when the issue went out inline or its round is still
to be posted. So rounds that waited for approval post in any order
without contradicting each other: an older round posted after a newer
one says the newer one's verdict, the same answer is never said twice
(two rounds answering the same reply answer it once), and an issue a
later round settled while its own round waited is answered and resolved
in the thread its own round's review opens. Only threads hunter opened
are ever answered or resolved, and each is found afresh, by its first
comment, on one read of every thread of the PR at post time — never by an
id kept from an earlier read, so a thread deleted since is recorded gone
and skipped, and the post goes on with the rest. A thread that read finds
holding news the verdict's round was not shown — a reply that would make
a round due, by the same rule (below) — is left as it is — no "Rebuttal
accepted" or resolution over an objection nobody judged — and what the
read found there is recorded, so the news awaits judgment at once, shown
on the review: on a watched review it makes the next round due, and on an
unwatched one, where news alone makes no round due, a request asks for the
round that judges it (as it does for news no post read: a request reads
the threads itself). Each step is recorded as it lands, so a
post that failed partway retries only what is missing; a failed post
counts toward the round's 3 attempts like any other. So does a resolve or
unresolve the forge fails, whatever it answers (GitHub's secondary rate
limit is a 403): hunter's login can push, so the forge never refuses it a
thread's resolution for want of access, and the retry takes up where the
post stopped. A forge call that failed may still have acted (a timeout,
a 5xx after the write), so a retry never takes a step again on the
strength of the failure alone: each answer and each review carries a
hidden mark naming its round, and the retry looks for it first — the
answer in the thread it reads anyway, the review among the PR's reviews
(GitHub) or notes (GitLab) by the operator's login, read only when an
earlier post sent it (`review_sent_at`) — and records what it finds
instead of saying it twice; a thread already showing the resolution a
step wants is recorded so, not set again. GitLab publishes a review's
inline notes one by one before its body, so each also carries a hidden
mark naming its issue: the retry takes a note on the MR as an issue's
only by that mark, never one of another issue that reads the same (of
the same round, published before the failure, or of an earlier one).

Watch also acts on replies and reopens alone. For every watched open
review with posted issues, the PR sync reads their threads (one GraphQL
query or `discussions` read per review, as a round does; a review with
none costs no call), so a thread hunter resolved that a human reopened
reopens its issue without waiting for a push. It records the screened
replies on the issues, with the newest by a trusted author other than
hunter (`newest_reply_at`). An issue *awaits judgment* (`awaits_since`,
derived in the `pr_review_issue_state` view) while it holds news no
verdict was given over: a reopen no round judged since, or a reply newer
than the newest one its newest verdict was shown — any reply, on an issue
no round judged — unless hunter's own resolution of the thread stands: a
reply under it asks for nothing (one that disagrees reopens the thread),
while one in a thread somebody else resolved is news all the same
(GitLab's *Comment & resolve thread* is a rebuttal and a resolution in
one action). The post's check above is this rule over its own read. An
issue a reviewer left out stays as it was, never settled by
silence, and its news still awaits; so does every issue of a round whose
verdict file is missing or unreadable, which records none. While the
PR's head is still the reviewed one, a watched review with an issue
awaiting judgment is due once the newest such news has been quiet for 2
minutes (`REPLY_QUIET_MS`; a burst of replies is one round), and any
review is due at once when a request asks for that round: an unwatched
review's threads are read only by its rounds, its posts and requests, so
its news brings no round by itself; a request reads them before it
decides, and at the reviewed head asks for the round when news awaits. That round
has no commits to review (`base == head`, shown as *re: replies*): the
reviewer is told so, judges the earlier issues against their replies and
the code, and reports a new issue only if a reply reveals one. Only a
verdict on an issue awaiting judgment is progress, so a round of this
kind that gives none — no readable verdict file, every such issue left
out, or stopped by its cap before judging one — fails toward the hold
rather than being due again at once with the same prompt; one stopped
after judging some is kept (as partial, which ends the streak: it made
progress, and the hold is for rounds that make none), and
the rest still await. It records its verdicts and is posted like any
round, and leaves `last_reviewed_sha` where it was. What makes a round
one for replies is why it is due — the head is the reviewed one — never
an empty range: a round for a new head is that head's. A head with no
commits past its base (a first round of a PR without any, or a watched
PR force-pushed back to its merge-base, whose earlier issues wait for
code, a reply or a reopen) runs no reviewer and is recorded as an empty
successful round, so the head does not keep the review due; news
awaiting judgment stays so and, the head being reviewed, brings a round
for it alone. A new head with pending replies is one ordinary round,
which reads the replies as well; a round that finds the head moved since
the pick skips, and the new head waits out its stability window. A round
reviews exactly the head the forge reported: when the fetched
`refs/pull/N/head` (`refs/merge-requests/N/head`), which the forge
updates some time after a push, still names another commit, the round
fails as a failed fetch does and is retried after the error backoff,
never reviewing or recording that other commit. A forge error in the
sync's read is logged and changes nothing; failed rounds, holds, and
budget or offline denials count as for any round.

## Web UI

Served at `http://localhost:8377` (configurable). Left-nav pages:

| Page | Shows |
|---|---|
| **Status** | Budget bars (used + available, per window), Run/Pause and Overdrive controls, what the scheduler is doing right now / why it isn't, and the recent event log |
| **Inbox** | New findings awaiting triage, with per-type filters |
| **Kanban / Pipeline** | Findings in flight in foldable columns (folds remembered per browser), a **Held** column with held findings (why each is held, the report, and its Continue/Restart/... controls), a Held scans section for repos with held work, suppressed (rejected/wontfix), and informational (note) findings |
| **All Findings** | Every finding, filterable by repo/type/status/severity, with full detail (jobs, PR state, timeline) on expand, and the actions its status allows (verdicts on new/note/closed, releases of held work, Recheck on new, Unqueue on queued, budget override while something will still pick it) |
| **Repos** | Add/remove/pause repos; each repo's held scans and failing sync with why and the way out (**Continue**, **Restart**; **Retry now** for a held sync), and the scans failing without a hold yet; queue a full re-hunt (the next hunt reviews the repo's complete history instead of only new commits — the same scope as the periodic `hunt.rehuntDays` re-hunt — and the request stays pending until such a hunt finishes cleanly); per-repo notes (free-text context injected into every prompt for that repo — coding conventions, known false positives, anything worth a hunter remembering across runs) |
| **Reviews** | Reviews of PRs hunter did not open: request one by URL or number, or pick from the repo's open PRs (with **Watch**, every new head is reviewed again once it has been stable for 10 minutes, and replies or reopened threads no round has judged bring a round of their own, shown as *N issues to judge again — review queued* with the time of the newest such news; unwatched, a request reads the threads, shows their news and asks for its round; with **Post automatically**, rounds go to the PR without approval); an author or fork owner who cannot push needs a vouch, the dialog naming them and the fork whose code would run; a review whose fork changed hands since says so and waits for a request; each review's state, auto-post toggle, rounds (base..head, or *re: replies* for a round without new commits; verdict, issue, resolution and answer counts, post state with **Post to PR** / **Discard** for a pending one (*partly posted* and no **Discard** once part of it is on the PR) and the last post error) and issues (marked *inline* once posted as a diff comment; status open / addressed / rebuttal accepted / reopened, the newest verdict's note, *resolved on forge* once hunter answered and resolved the thread, and the thread's screened replies with a count of hidden ones), with **Retry** for one held after 3 failed rounds |
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
deep-linked list explicit; future options are not silently admitted.
Checking again every option you unchecked from All returns the dimension to
all. Use the All control to return to an unrestricted selection. Defaults
are omitted when controls update the URL.

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
| GET | `/api/stats` | Aggregate job stats; `horizon` = `24h` \| `7d` \| `30d` \| `90d` \| `all` (default) counts only jobs started in that window |
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
| GET | `/api/repo/open-prs` | A repo's open PRs from its forge (`id`), marked when hunter opened them or a review exists |
| GET | `/api/pr-reviews` | Requested PR reviews with their rounds and issues, newest request first |
| POST | `/api/pr-review` | Request a review (`{"repo_id", "pr": URL or number, "watch", "auto_post"?, "vouch"?}`); 409 `needs_vouch` (with `vouch_for` and the fork's `head_owner`) when the author or the owner of the fork the head lives in cannot push, 409 for hunter's own PR; for an existing review, sets its `watch` and `auto_post`, reads the threads of its posted issues (400 when that read fails) and asks for a round at the PR's head unless a round already reviewed it and nothing awaits judgment (`outcome`: `requested`, `updated`, `reopened` or `rebound`) |
| POST | `/api/pr-review/watch` | Turn a review's Watch on or off (`{"id", "watch"}`); on also clears a hold |
| POST | `/api/pr-review/retry` | Clear a review's failures and hold (`{"id"}`) |
| POST | `/api/pr-review/auto-post` | Post a review's future rounds without approval, or stop (`{"id", "auto_post"}`) |
| POST | `/api/pr-review/post` | Approve a pending round for posting (`{"round_id"}`), also after failed posts (clears the error); wakes the PR sync; 409 unless pending and the PR open |
| POST | `/api/pr-review/discard` | Never post a pending round (`{"round_id"}`); 409 unless pending, or once part of it is on the PR |

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
(`NOT-A-BUG.md`, `PR-DESCRIPTION.md`, `WITHDRAW.md`, ...). The pre-PR
reviewer is the one worker that writes nothing in its tree: its issues go
to `review-<job>-<round>.json` beside the tree, in the chain's workspace
root, and its transcript to `review-<job>-<round>/` there.

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
- **`pr_reviews`**, **`pr_review_rounds`**, **`pr_review_issues`**,
  **`pr_review_verdicts`** — requested reviews of PRs hunter did not open
  (PR, the repo URL it was requested under and its binding to it, author,
  Watch, auto-post, the trust decision (vouches, the fork owner decided
  about) and the fork owner read last, newest and last reviewed head,
  the request still awaiting its round (when, and the head it read), failure streak and hold, and
  whether a post may have left drafts on the PR), each attempt's
  `base..head` (equal for a round run for replies and reopens alone) with
  the merge-base it resolved and the binding it ran under,
  verdict (`ok`, `partial`: stopped before it finished, or `failed`) and
  post state (`pending`, `approved`, `posted`,
  `discarded`; error, attempts, posted time, when a post last sent its
  review, the forge's review id, and
  when part of it first landed on the PR), the
  issues a round reported (with the `side` their line numbers; once posted
  inline, the comment's forge id once read back and when its thread was
  found gone, the thread's screened replies with their times and hidden
  count, the newest trusted reply in it, and
  what hunter said there: its standing answer, the newest reply it
  answered, when it resolved the thread, or saw it reopened), and each
  later round's verdict and note per earlier issue, with the newest reply
  and the reopen it was given over and the reply it answers when the
  thread awaited one. An issue's status is not stored: the
  `pr_review_issue_state` view derives it, the verdict its thread is to
  show and whether it awaits judgment, from the verdicts.
  `jobs.pr_review_id` names the review a `pr_review` job ran for.
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
  `fix` override per job family; `null` inherits `default`. The pre-PR
  reviewer runs with the `fix` model.
- `review.maxRounds` (default 7) — how many times a finished fix is
  reviewed before it ships; issues still open after the last round hold
  the fix for you (§ Finding lifecycle). `0` ships unreviewed.
- `review.curate` (default true) — once a review ends in a ship with more
  than one commit, the fixer rewrites them into a few logical commits of
  the same tree (`playbooks/curate.md`); a rewrite that changes the tree
  or fails is thrown away and the original commits ship.
  Each reviewer and each answer gets its own `fix.maxWallS`; the loop as
  a whole is bounded by the rounds and the token grant, and a stopping
  daemon starts none of its runs.
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

Upgrading to pushes over the stored URL: fix and engage pushes used to
rewrite an `http(s)://` repo URL to its ssh form, and now go to the URL as
stored. A repo registered over https whose pushes relied on SSH keys fails
to publish until its URL is switched to the ssh form, and one stored as
`http://` (no longer accepted) wants `https://` or ssh. `POST /api/repo`
with `{"id": <id>, "url": "git@<host>:<owner>/<repo>.git"}` changes it and
repoints the clone.

It idles at zero token cost between cycles and wakes on a smart-sleep
policy: right after a job with more queued work → drain immediately;
budget denied → short retry if the ramp is actively rising, otherwise sleep
to the window reset; genuinely idle → 15 minutes, or until the next PR
review round comes due, if sooner. PR-comment polling is
decoupled from the token-budget backoff loop, so a long budget-driven sleep
never delays noticing new PR feedback. Every wake re-checks the budget gate
before spending anything, and starts no job while the filesystem holding
`workRoot` has less than 1 GiB free (retried every 5 minutes), or while
the picked work's forge or provider is unreachable (retried every 3
minutes).

