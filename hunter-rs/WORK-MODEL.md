# WORK-MODEL.md: pausing, holding and giving up

**Status: implemented** (#88), as described below, with the deviations listed in the PR that implemented it. This describes the model for how per-finding work gets paused, held for an operator, and given up; §1–§4 describe the problems it replaced. Repo-level work (hunt and the analysis scans) is on the same model since #92; §17 lists only what differs for it.

It came out of the review of #56. That PR fixed one runaway loop, but it showed that "blocked", "suspended" and "stuck" are each represented differently for every kind of work, in places that don't agree.

---

## 1. The problem today

| Symptom | Where |
|---|---|
| "Held" only exists for fix. It is three separate facts: `findings.status = blocked`, `jobs.state = suspended` and `jobs.blocker IS NOT NULL`. Each place that needs to know evaluates its own combination of them. | `block_fix_job`, `held_fix_blockers`, `list_resumable_jobs`, `retire_stranded_suspensions`, `resume_plan`, `prepare_blocked_resume` |
| `jobs.blocker` is copied into every resumed attempt and never cleared. Once a chain has been held, every later attempt carries the text. | `create_job` |
| "An operator requeued this" is inferred from `blocker` being set. A daemon restart in the middle of a resume, or a `release_claim` before the worker runs, makes the same inference true without any operator, and the chain resumes past the give-up ceiling. | `resume_plan` (`operator_requeued`), `requeue_stranded_fixes`, `release_claim` |
| What a hold is about (a missing prerequisite vs. a failure streak) is told apart by the `stuck: ` prefix of free text. | `streak_hold_report`, `is_streak_hold` |
| Job rows are records of attempts, yet they are rewritten after the fact: give-up flips `suspended` → `failed`, a hold flips a `failed`/`give-up` row back to `suspended`, the sweep marks rows `finding-moved`. | `retire_suspended_job`, `block_fix_job`, `retire_stranded_suspensions` |
| A recheck that keeps failing goes back to `new` with no reason, and looks exactly like a fresh finding. | `handle_recheck_failure` |
| A harvest given up on a **merged** PR has no way back: `merged` doesn't accept a verdict, no harvest endpoint exists, and the UI shows nothing. | `record_harvest_failure`, `FindingStatus::awaits_verdict` |
| An engage that fails without suspending (the worker doesn't reach `Done`, the push or the reply fails) leaves the attention flag set. Engage is the highest finding tier, so the PR is picked again every cycle, with no counter. | `record_engage` (Err arm), see the comment in `withdraw_pr` |
| A given-up engage chain only sets the PR's attention aside. That is visible as an `error` event, not as a state. | `count_given_up_chain`, `Store::set_attention_aside` |
| A `backend.run` error isn't counted for any kind. The cycle fails, and the same work is picked again 5 minutes later. | `run_*`, `daemon::compute_sleep_s` |
| A tree that can't be made (`open_workspace` fails) isn't counted for harvest and engage. The work stays in its tier and is picked again every cycle. #5 fixed this for fix and recheck. | `run_harvest`, `start_engage` |

---

## 2. Terms

| Term | Meaning |
|---|---|
| **Finding** | A reported defect or improvement, with a lifecycle status (`new`, `queued`, `pr_open`, `merged`, …). |
| **Kind** | One kind of per-finding work: `recheck`, `fix`, `engage`, `harvest`. |
| **Work item** | One piece of work: a finding × a kind. It is **active** while the finding is in that kind's tier (§6.1). |
| **Attempt** | One worker run, i.e. one `jobs` row. |
| **Chain** | Attempts linked by `jobs.resumed_from`, continuing the same transcript and tree. |
| **Paused** | A work item with a checkpoint and no hold. The scheduler continues it on its own. |
| **Checkpoint** | The attempt whose transcript and tree the next attempt continues from. |
| **Failure** | An attempt, or a step before one, that ended without the work's outcome and without a pause (§7). |
| **Streak** | The number of consecutive identical failures of a work item. |
| **Ceiling** | The bound on how far the scheduler continues a chain on its own (`MAX_RESUME_ATTEMPTS`, `GIVE_UP_MULTIPLE`). |
| **Hold** | A work item stopped for an operator. It has a typed reason and a release. |
| **Release** | An operator's decision that ends a hold, recorded with who, when and which action. |

---

## 3. Principles

1. **Control flow reads state, never history.** Explicit state fields are what decisions use. History records what happened: it is never rewritten, and nothing decides from it. Every state change appends its history entry in the same transaction.
2. **Every kind is treated the same.** Each kind can pause, fail, get stuck and be held, through one shared mechanism. Kinds differ only in what counts as their outcome, what counts as a failure, and which release actions make sense.
3. **Every automatic retry is bounded.** By the streak, by the ceiling, or both. Nothing is retried every cycle without bound.
4. **Every give-up is visible and recoverable.** It ends in a hold with a typed reason and an explicit way out. It never ends in a silent status change or a flag.
5. **Operator decisions are data.** A release is a row, not something inferred from a leftover column.
6. **Each question has one answer in one place.** "Is this work item held?" and "what does it continue from?" are each one query, used by every tier, the preview, the sweep and the API.

---

## 4. Facts, state and derived values

```mermaid
flowchart LR
  subgraph Facts["Facts: append-only, never rewritten"]
    J["jobs: one row per attempt, how it ended, spend, transcript"]
    E["events: human-readable log"]
  end
  subgraph State["State: small, mutable, constrained by the schema"]
    F["findings.status: lifecycle"]
    W["finding_work: streak, checkpoint"]
    H["work_holds: active row = held"]
  end
  subgraph Derived["Derived: computed, never stored"]
    C["chain attempts and spend since the last release"]
    N["needs-you list"]
    T["timeline"]
  end
  J --> C
  H --> C
  H --> N
  J --> T
  E --> T
  W -. "checkpoint_job_id" .-> J
```

- **Facts.** A `jobs` row is written when the attempt starts and completed once, when it ends. After that, nothing changes it: not a give-up, a hold, or a sweep. `jobs.state = suspended` means only "this attempt ended paused". It no longer means "this is resumable".
- **State.** It is small enough that the schema can enforce its invariants (a unique index for "at most one active hold").
- **Derived values** are fine as long as they aggregate immutable facts plus state. The problems in §1 come from deriving decisions from copied or rewritten data.

A `work_holds` row is both state and history. The active row (`released_at IS NULL`) is the current hold. Released rows are the record of past holds and their releases. Rows are only ever appended, and releasing one only fills in its `released_*` columns.

## 5. The finding lifecycle

This section walks a finding from the moment a scan files it to every terminal state, in the target model. It shows where each kind of work happens, where holds can occur, and who causes each transition. Where today's code differs, the difference is called out.

### 5.1 Status groups

| Group | Statuses | Owned by | Work item active |
|---|---|---|---|
| Inbox | `new`, `note` | the operator | none |
| Before a PR | `rechecking`, `queued`, `fixing` | the scheduler | recheck, fix |
| PR | `pr_open` | the forge and engage | engage |
| After the PR | `merged`, `closed`, until harvested | harvest | harvest |
| Terminal | `merged` once harvested, `superseded`, `rejected`, `wontfix` | nobody, except the reopen paths in §5.3 | none |

`blocked` no longer exists: a held fix stays `queued`.

### 5.2 Main path

```mermaid
stateDiagram-v2
  direction TB
  state decline <<choice>>
  state classify <<choice>>
  [*] --> new: a scan files it
  new --> rechecking: operator Recheck
  rechecking --> new: verdict confirmed
  rechecking --> superseded: verdict stale
  rechecking --> rejected: verdict invalid
  new --> queued: operator Queue
  queued --> fixing: scheduler claims it
  fixing --> queued: attempt paused or failed
  fixing --> pr_open: PR shipped
  fixing --> decline: worker declines
  decline --> superseded: superseded, duplicate, obsolete
  decline --> rejected: wrong
  decline --> wontfix: unwanted
  decline --> new: unclassified decline
  pr_open --> pr_open: engage replies, pushes, or finds nothing to do
  pr_open --> merged: forge reports merged
  pr_open --> closed: forge reports closed, or engage withdraws
  merged --> [*]: harvest reviewed it, stays merged
  closed --> classify: harvest classifies
  classify --> superseded: superseded, duplicate, obsolete
  classify --> rejected: wrong
  classify --> wontfix: unwanted
  classify --> new: abandoned
  superseded --> [*]
  rejected --> [*]
  wontfix --> [*]
```

While a finding sits in `rechecking`, `queued`/`fixing`, `pr_open`, or `merged`/`closed` before harvest, the matching work item runs through the shared machine in §6.2: it can be idle, running, paused or held. None of that changes the finding's status. The only status moves the scheduler makes on its own are the ones drawn above, plus `fixing` ↔ `queued` around each fix attempt.

Two edges need a word:
- **Engage** changes the status only when it withdraws (`WITHDRAW.md` → `closed`). Everything else it does (replying, pushing review fixes, deciding there is nothing to do) leaves the finding `pr_open`; that's the self-loop. §5.4 draws engage in full: what raises the attention flag, each outcome, and its hold.
- **Back to `new`:**
  - A **declined fix** goes back to `new` when its decline file has no valid classification (`parse_decline`). The fix playbook's list doesn't offer `abandoned`, but the parser accepts it, so a fix worker writing it would land the finding in `new` too.
  - A **closed PR** goes back to `new` when the harvest classifies its closure as `abandoned`, which `harvest-closed.md` does offer.
  - In both cases the outcome says nothing about the finding, so it goes back to triage (`Unclaimed::from(ClosureClass)`).

### 5.3 Side transitions

These come from the operator, from later scans, and from the dep-update machinery:

```mermaid
stateDiagram-v2
  direction LR
  state "note" as noted
  new --> rejected: operator verdict
  new --> wontfix: operator verdict
  new --> noted: operator verdict
  noted --> queued: operator verdict
  noted --> rejected: operator verdict
  noted --> wontfix: operator verdict
  closed --> queued: operator verdict
  closed --> rejected: operator verdict
  closed --> wontfix: operator verdict
  closed --> noted: operator verdict
  queued --> new: operator Unqueue
  rejected --> new: a scan refiles it after its anchor changed
  new --> superseded: dep update no longer proposed
  superseded --> new: dep update proposed again
  queued --> superseded: dep update handed over to its replacement
  pr_open --> queued: pr_open without a PR URL, legacy rows
```

- **Every edge is guarded:** every status write lands only while the finding is still at the status its writer read, so no writer overwrites a move made between its read and its write. Every edge outside a fix claim is a `transition::Transition` that names it, made by `Store::transition`, or inside a writer's own transaction when it lands with other writes (a hold's release, a closed harvest's stamp, a dep-update handover's other half, which refuses the whole handover if either half conflicts). The variant fixes its sources, one status or either of two (`Sources`); an operator's verdict (`OperatorVerdict`, built only from a status that awaits one) applies only while the finding is still at the status `/api/verdict` read before checking it, and replaces the earlier verdict's reason and anchor even when it gives no reason. What a transition records with the status (a verdict's reason and anchor, a confirmed recheck's revised analysis) lands in the same guarded write. A PR's end (`pr_open → merged`, `pr_open → closed`) is not a `Transition`: a status cannot tell the PR its writer read from the one the finding shipped next, and `pr_state` has one row per finding. `Store::end_pr` takes the PR (`PrRef`: the URL the writer read and its number) with the `PrEnding`, and writes the move and the end in `pr_state` together, only while the finding is still `pr_open` with that PR, checked under the write lock; `PrEnded::OtherPr` otherwise. So a finding is never `merged` or `closed` over a `pr_state` that does not say so, and an old PR's end never closes the finding under its next PR. A `Moved::Conflict` (for a PR's end, `PrEnded::Conflict` or `PrEnded::OtherPr`) leaves the other decision standing: the endpoint answers 409; the PR sync neither records the PR merged or closed nor counts it; a recheck's verdict is dropped whole (no analysis, outcome or verdict event); and an engage withdrawal hands nothing to the harvest, so one closure can't be harvested twice. A hold's release (`HoldReleased`) uses the status read with the hold in the same transaction. The only status writes outside `Transition` are a PR's end (`Store::end_pr`, above) and a fix claim's own moves (below). The unguarded writes (`set_finding_status`, `set_finding_verdict`, `set_anchored_verdict`, `set_finding_pr_open`, and the `pr_state` ends `mark_pr_merged` and `mark_pr_closed`) exist only in a `test-support` build, as fixtures.
- **Reopening:** `rejected → new` is `Transition::Reopened`, used when a worker-anchored rejection's files changed and a scan files the finding again (`verdict_anchors`). `wontfix` has no anchor and is never reopened.
- **Daemon restart, or a fix claim dropped unreleased:** `fixing → queued` (`requeue_stranded_fixes` at startup, `requeue_stranded_claim` after a cycle) is the same move as `FixClaim::release`. It isn't a separate status path, and it spares a claim a `running` job still holds. At startup `recovery::reconcile_and_log` releases every `fixing` claim; the global form takes the startup token (`Unreconciled`), so nothing can run it beside a live fix. While the daemon runs, a claim is a value: `CycleScope::claim_fix` returns it as a `FixClaim`, and only that value moves the finding out of `fixing` again (`release` → `queued`, `open_pr` → `pr_open`, `conclude` → a declined fix's verdict, which takes the decline's classification, so only `transition::decline_status`'s statuses can be written), each consuming it. Each writes only while the finding is still `fixing`; `open_pr` and `conclude` return `Moved`, and a `Conflict` (the finding left `fixing` meanwhile) records nothing but an event naming where it went. The store's writes behind them take an `Owner` only `crate::recovery` makes, and no `Transition` can name `fixing` as a source or a target: its statuses are `transition::Unclaimed`, which has no `fixing`. A `FixClaim` dropped while still holding the claim (its fix returned early, or its release failed) owes the finding to the store handle's ledger, and `CycleScope::settle` puts it back to `queued` once no `running` job works on it; until then it stays owed. A released claim is gone, so a later claim of the same finding, by this cycle or another, has its own holder and no settle touches it. The ledger names each dropped claim, not its finding, so a later claim of the same finding, dropped while a settle that released the earlier one is still running, stays owed after that settle. A settle that fails (a store error) is the cycle's error: `scheduler_state` reads `error` and the loop backs off 5 minutes. What it failed on stays owed, and every cycle first retries that (`Recovered::settle_unsettled`); while the retry fails, no cycle starts. Repeating a settle is safe: it only acts on rows still `running` or `fixing` (`suspend_orphan` and `orphan_job` write only a job still `running`), and an owed claim stays `fixing` until released, so no other holder can claim that finding meanwhile.
- **A `running` job always has an owner:** a job row is created only `running`, by `CycleScope::create_job`, which returns it as a `RunningJob`. Only that value ends it (`complete` records the outcome, `fail_unstarted` a job whose worker never ran), each consuming it; the store's writes behind them take an `Owner` only `crate::recovery` makes. `complete` takes a `FinishedState` (`done`, `failed`, `killed`, `suspended`), which has no `running`, and returns a `RecordedJob`, which `JobStore::fail_job` requires, so revising an outcome cannot touch a job that is still `running`. A `RunningJob` dropped unended (its runner returned early, or the write that would have ended it failed) owes the job to the ledger, and the first settle that finds no process working in its tree suspends or kills it (`abandoned -- its runner returned without recording an outcome`). Dropped is not stopped: the worker runs on a blocking thread that a cancelled future does not stop, so while a process works in the tree the job stays `running` and owed (`Settled::deferred`), keeping its reservation; one still held is in no ledger and no settle takes it, however many scopes are live. Only `recovery::reconcile_and_log` makes the daemon's `Recovered`, the proof a scope is opened with; a store handle cannot be asked for one (`Store::recovered` exists only in `test-support` builds).

### 5.4 The PR phase

```mermaid
flowchart TD
  ship["fix ships a PR"] --> open["pr_open"]
  open --> sync{"sync_prs, the admin lane's PR sync duty"}
  sync -->|"merged"| merged["merged"]
  sync -->|"closed"| closed["closed"]
  sync -->|"new comments, changes requested, conflict, failing checks"| flagged["attention flag set"]
  sync -->|"nothing new, or the same reasons already handled at this head"| open
  flagged --> engage["engage work item"]
  engage -->|"reply, push, or nothing to do"| handled["mark engaged: watermark and handled reasons"]
  handled --> open
  engage -->|"WITHDRAW.md"| withdraw["close the PR"]
  withdraw --> closed
  engage -->|"pause"| engage
  engage -->|"3 identical failures, ceiling, BLOCKED.md"| eheld["engage held"]
  eheld -->|"continue or restart"| engage
  eheld -->|"dismiss"| handled
  merged --> mharvest["harvest work item"]
  mharvest -->|"reviewed"| done["merged and harvested"]
  closed --> charvest["harvest work item"]
  charvest -->|"classified"| cls["superseded, rejected, wontfix or new"]
  mharvest -->|"stuck, ceiling, BLOCKED.md"| hheld["harvest held"]
  charvest -->|"stuck, ceiling, BLOCKED.md"| hheld
  hheld -->|"continue or restart"| mharvest
  hheld -->|"continue or restart"| charvest
  hheld -->|"closed PR: classify by hand"| cls
  hheld -->|"merged PR: mark harvested"| done
```

- **Engage withdraws:** the PR is closed, and the harvest that follows continues the engage worker's transcript (`plan.handoff`), so it doesn't start cold.
- **The PR is merged or closed on the forge while engage is held or paused:** the finding leaves `pr_open`. The engage hold is released as `finding-left-tier`, and harvest takes over.

### 5.5 Statuses and work items together

Pausing, getting stuck and being held never change a finding's status (§9.5, invariant 4). They happen inside the status, in the work item for that status's kind. Two diagrams together describe this: §5.2 draws how the **statuses** move, and §6.2 draws the **work-item machine** (idle, running, paused, held) that runs inside every status hosting work. It is the same machine for every kind.

| Kind | Host status | Outcomes (they leave from **running**) | Terminal releases (they leave from **held**) |
|---|---|---|---|
| recheck | `rechecking` | confirmed → `new`; stale → `superseded`; invalid → `rejected` | `reject` → `rejected`; `wontfix` → `wontfix` |
| fix | `queued`, `fixing` while running | PR shipped → `pr_open`; declined → its classification (§5.2) | `reject` → `rejected`; `wontfix` → `wontfix` |
| engage | `pr_open` | withdrawn → `closed`; replied, pushed or nothing to do → stays `pr_open` | `dismiss` → stays `pr_open`, attention handled |
| harvest | `merged` until harvested | reviewed → `merged`, harvested | `mark_harvested` → `merged`, harvested |
| harvest | `closed` until harvested | classified → `superseded`, `rejected`, `wontfix` or `new` | `classify` → the same edges as the worker's classification |

How the two levels connect:
- **Outcomes leave from running.** An outcome is how an attempt ends; the attempt is what decides the next status.
- **Terminal releases leave from held.** The operator decides instead of a worker. `continue` and `restart` stay inside the work item (§6.2).
- **Outside events can leave from any state.** These are the operator unqueueing a fix, and the forge reporting the PR merged or closed. The finding leaves the tier, which releases an active hold as `finding-left-tier` and clears the checkpoint (§6.3).
- **In fix, `running` is the `fixing` status** and every other state is `queued`. Starting an attempt claims the finding, and the attempt ending releases the claim.
- **In engage, `idle` covers both "no attention flag" and "flagged, not yet started".** A fresh start needs the flag (§6.1).

### 5.6 Gaps the walk exposes

Walking every path turned up these. Each needs a decision before or during implementation.

| # | Gap | Today | Proposed |
|---|---|---|---|
| G1 | The engage tier is keyed on a flag that `sync_prs` recomputes on every run of the admin lane's PR sync duty | A paused engage is resumable while the finding is `pr_open`, whatever the flag (`list_resumable_jobs`), so today's resume gate already ignores the flag | Engage is active while `pr_open`; the flag only decides whether to start fresh (§6.1). |
| G2 | Dep-update handover and refresh vs. a fix that has a checkpoint or a hold | `hand_over_queued_dep_updates` supersedes `queued` sources and `refresh_dep_update` rewrites `new`/`queued` ones. Held fixes are safe only because they are `blocked`. A **paused** fix is `queued`, so its finding can be superseded, or its target rewritten under the checkpoint. Reproduced by `a_paused_fix_keeps_its_queued_update_from_being_handed_over` and `…_refreshed`. | Both skip a finding whose fix work item has a checkpoint or an active hold. Add a test for today's paused case first. A cold fix is exposed too: the admin lane's Renovate pass can refresh a `queued` row between the job lane's pick and the fix's claim, so the claim reads the row in its own write (`FixClaim::finding`) and the fix works from that row, never the pick's; once `fixing`, nothing rewrites it. |
| G3 | The engage → harvest handoff crosses kinds | The first harvest job is created with `resumed_from` = the engage job | A work item's checkpoint may point to another kind's job for the first attempt only. The handoff stays a special case in the plan, not a general cross-kind chain. |
| G4 | Operator actions on held work aren't verdicts on inbox statuses | `/api/verdict` only accepts `new`, `blocked`, `note`, `closed` | Release actions go through `POST /api/hold/release` (§12). `/api/verdict` keeps the inbox statuses and `closed`, and no longer accepts `blocked`. |
| G5 | The forge merges or closes the PR while an engage attempt is **picked or running** | `sync_prs` runs on the admin lane, beside the job lane (`crate::daemon`), so it can record the merge between an engage's pick and its job, or while it runs: `Transition` moves the finding out of `pr_open` and the PR's end lands in the same write. An engage picked before that is refused at its insert (`JobInsertError::NoEngageDue`, read under the insert's write lock) and the cycle is skipped, so no worker starts on an ended PR. Nor does one the sync has not caught up with: an engage, fresh or resumed, reads its PR's state from the forge, and if it is merged or closed the cycle ends before any job row, changes nothing (the PR's end is the sync's to record), spends a one-shot override, and sets `CycleSummary::awaits_pr_sync`, on which the job lane holds that engage back from its pick until a sync pass begun after it has read every open PR (`daemon::EngageWaits`, `scheduler::EngageHoldback`), and picks other work meanwhile. If that pass still has not recorded the end (its view or its write failed for this PR, which it logs), the job lane counts an engage failure (`scheduler::PR_END_UNRECORDED`, `count_unrecorded_pr_end`): otherwise the engage would be picked again, view the PR and wait for another pass without end. The third identical one holds the engage (§7). Until the first sync pass since startup has finished, every engage is held back the same way, so a merge made while the daemon was down is recorded before an engage is picked; the job lane itself never waits for the sync, so no other tier waits for a slow forge. One already running reads the finding again when its worker ends: if the PR it answers is no longer the finding's open PR, nothing is pushed, posted or withdrawn, nothing is counted as a failure, and a one-shot override is spent (outcome `not published: PR ended`). A `BLOCKED.md` is still offered to the hold, which refuses a finding out of the tier, and that is the attempt's outcome, not the cycle's error. If it pauses, it gets no checkpoint: `point_checkpoint` joins `work_tiers` on the status at the moment it writes. The harvest the merge calls for waits until the engage's job ends, since a finding has one running job at a time (migration 031). What remains is the seconds between that re-read and the push or reply landing, on the forge side only. | Accept. |
| G6 | A PR closed on the forge and then reopened | `sync_prs` only revisits `pr_open` findings, so a reopened PR stays `closed` in hunter. [INFERENCE] | Out of scope; listed so it isn't forgotten. |
| G7 | What a by-hand harvest of a merged PR means | Nothing can retry or skip a given-up merged harvest | For `merged`, the action is "mark harvested", not "classify". It records that the PR was never reviewed. |
| G8 | Re-entering a tier | A finding that returns to `new` (abandoned, confirmed) and is queued again opens a new PR. `pr_state` streaks reset only when `pr_number` changes. | Leaving a tier resets the work item's streak and checkpoint (§6.3), so a second PR starts clean whatever its number. |
| G9 | A tree that can't be made, before any worker runs | `open_workspace` fails the job and the runner returns without counting. Harvest and engage outrank fix, so either of them starves everything below. #5 counts it for fix and recheck under the fixed key `workspace not created`; harvest waits for its hold (step 6), because a harvest given up today can't be recovered. | A failure for every kind (§7), with the fixed key and git's reason as detail. The hold that follows has no checkpoint: no worker ran. |

---

## 6. The work item

### 6.1 Tiers

A work item is **active** while its finding's status belongs to the kind. Within that, the work item is **eligible to start fresh** only under the kind's start condition. Resuming a checkpoint needs only the work item to be active; this matches the gate `list_resumable_jobs` uses today.

| Kind | Active while | Starts fresh when |
|---|---|---|
| recheck | `findings.status = rechecking` | always |
| fix | `findings.status IN (queued, fixing)` | `queued` |
| engage | `findings.status = pr_open` | `pr_state.needs_attention IS NOT NULL` |
| harvest | `findings.status IN (merged, closed)` and `pr_state.harvested_at IS NULL` | always |

The attention flag is deliberately **not** part of engage's active condition. It is derived, never written by hand: every write of what sync observed on the PR, or of what an engage answered, settles it from the row in the same transaction (`crate::attention`). It follows the PR, so it can flap (a flaky check going red and green). If the flag decided whether engage was active, the PR losing its flag for a cycle would count as the finding leaving the tier: it would release an engage hold and drop a paused engage's checkpoint (§5.6, gap G1).

The tier order is unchanged: budget override, then engage, harvest, recheck, fix, then repo-level resumes and rotation. A **held work item is skipped by its tier**, using one shared query.

### 6.2 State machine

Every kind follows the same machine:

```mermaid
stateDiagram-v2
  direction TB
  [*] --> Idle: finding enters the tier
  Idle --> Running: start fresh
  Paused --> Running: resume from checkpoint
  Running --> Paused: attempt ended paused, sets checkpoint
  Running --> Idle: failure, streak below limit, clears checkpoint
  Running --> Held: failure reaches the streak limit
  Running --> Held: worker reports BLOCKED.md
  Paused --> Idle: ceiling reached, counts as failure
  Paused --> Held: ceiling reached on the last failure of the streak
  Held --> Paused: release continue
  Held --> Idle: release restart
  Running --> [*]: work concluded
  Held --> [*]: release with a terminal action
```

Not drawn: a work item in any state ends when the finding leaves the tier (a verdict, a merge, …). An active hold is then released as `finding-left-tier`, and the checkpoint is cleared.

The states aren't stored as an enum. They are derived from the work item's fields:

| State | Derived as |
|---|---|
| Running | a `running` job exists for the (finding, kind) |
| Held | an active `work_holds` row exists |
| Paused | not held, `checkpoint_job_id IS NOT NULL` |
| Idle | none of the above, finding in the tier |

A finding has at most one `running` job, whatever the kind, and repo-level work at most one per (repo, kind). The schema enforces both (migration 031, §10), so a second attempt beside a live one is refused at insert (`JobInsertError::Occupied`, naming the holder) instead of being scheduled. Selection never offers such a place: every finding tier, the resume tier and the repo rotation skip a place a `running` job holds (`Store::running_places`), as they skip held work, so a job a stray worker keeps `running` stops only its own place.

A successor names only the attempt it continues (`JobStart::Resume` / `JobStart::Handoff`), and `insert_job` reads its place from that row in the insert's transaction: a resume runs on its checkpoint's own work item, and only while that job is still the work item's checkpoint; a handoff is its engage's finding's harvest, and only while that engage has no successor (G3). Anything else is refused as `JobInsertError::NotContinuable`, which skips the cycle: selection names a checkpoint by the same pointer, so only a write between the pick and the insert (an operator restarting the work, another attempt continuing it) parts them, and the next selection reads where the work is now. A harvest, however it starts — fresh, resumed or handed over — goes in only while its finding's PR still awaits one by the harvest queue's own rule (merged, or closed while the finding is `closed`, and not harvested yet), read under the insert's write lock; otherwise it is refused as `JobInsertError::NoHarvestDue`, a skip. So a cold harvest that finished between an engage's withdrawal and its handoff leaves the handoff nothing to harvest again. With migration 031's indexes, an attempt therefore has at most one live successor, and never one on another finding or repo.

### 6.3 Transitions

| Trigger | Streak | Checkpoint | Hold | Event |
|---|---|---|---|---|
| Attempt ends with the work's outcome (§7) | reset | cleared | – | kind's own |
| Attempt ends paused (`job_state` = `suspended`) | unchanged | set to this job | – | `resume` |
| Failure of an attempt, streak below `MAX_CONSECUTIVE_SAME_FAILURE` | +1, or 1 if the failure differs | cleared (the attempt ended without pausing) | – | kind's own |
| Failure without a new attempt (fetch, PR view, runner error), streak below the limit | +1, or 1 if the failure differs | kept: no work replaced it | – | kind's own |
| Failure reaching the limit | reset | the failing attempt if it has a transcript, else the checkpoint the work item already had | `stuck` | `hold` |
| Ceiling reached | counts as the failure `resume chain gave up` | cleared, unless this creates a hold | `gave_up` if the limit is reached | `hold` or kind's own |
| Worker wrote `BLOCKED.md` | reset | set to this job | `prerequisite`, with the report as detail | `hold` |
| Release `continue` | – | kept | released | `release` |
| Release `restart` | – | cleared | released | `release` |
| Release with a terminal action (§9.3) | – | cleared; a harvest's `FOLLOW-UPS.json` in it is filed first | released | `release`, `verdict` |
| Finding leaves the tier | reset | cleared | released by `finding-left-tier` | `release` |
| Daemon restart (`recovery::reconcile_and_log`), or a cycle's runner that returned without recording its job (`CycleScope::settle`): a `running` attempt that did work | unchanged | set to that job | unchanged | `resume` |

---

## 7. Failures and streaks

What counts as the outcome and as a failure, per kind. Pauses never count as failures.

| Kind | Outcome (work concluded) | Failure |
|---|---|---|
| recheck | a verdict: confirmed, stale or invalid | tree not made, no or invalid verdict file, worker not `Done` without a pause, runner error |
| fix | a PR shipped, or a decline (`NOT-A-BUG.md`/`DECLINED.md`) | tree not made, nothing to ship, push or PR failure, worker not `Done` without a pause, runner error |
| engage | the reply posted and/or the push done, or a no-op engage, or `WITHDRAW.md` carried out | tree not made, worker not `Done` without a pause, push failure, reply failure, PR close failure on withdraw, any pre-worker error (`engage_target`, PR view), runner error |
| harvest | a classification recorded, or the merged PR reviewed | tree not made, PR view or diff failure (before a job exists), unusable `CLOSE-REASON.json` or verdict, worker not `Done` without a pause, runner error |

"Tree not made" means `open_workspace` failed. It is counted under the fixed key `workspace not created`, with git's reason as the detail. The reason can't be the key, because it quotes the job's own tree path, which differs on every attempt and would reset the streak each time (G9).

Rules:
- **Identical failures count.** As today, the failure key decides "identical". A different key resets the streak to 1. Known limit: only identical failures are bounded; failures that vary are retried every cycle.
- **One limit for every kind:** `MAX_CONSECUTIVE_SAME_FAILURE = 3`.
- **A failure that can't be written ends nothing.** It is logged, and the work item stays as it was, as `record_*_failure` already does since #56.
- **A failure counts only while its work item is active.** `Store::record_work_failure` counts nothing for a finding outside the kind's tier, or a harvest whose PR is harvested already: that work is over, and counting it would open the streak of the finding's next work item of that kind (G8). PR sync runs beside the job lane, so this is ordinary: an engage's PR can end while its worker runs.
- **A ceiling give-up is a failure** with the key `resume chain gave up` and the reason as detail, as #56 introduced.
- **The streak lives on the work item.** It replaces `findings.fix_attempts`, `findings.recheck_attempts`, `pr_state.harvest_attempts` and their `last_*_failure` columns, and engage gets one.

What is **not** a failure: a budget, disk-gate or network-gate denial, operator pause, or a pause by the worker. These are scheduler-level waits; they don't belong to one work item.

---

## 8. Checkpoints and the ceiling

- **The checkpoint is a pointer** (`finding_work.checkpoint_job_id`), not a property of job rows. Giving up, holding, or the finding leaving its tier clears or keeps the pointer; job rows don't change.
- **Every kind can resume from a checkpoint.** This includes recheck, engage and harvest, for both automatic pauses and holds.
- **The ceiling counts what the scheduler did on its own.** It counts attempts and spend in the chain **started after the work item's last release**. Before any release, that is the whole chain.
  - An operator's `continue` therefore gives a fresh budget.
  - A daemon restart, a failed write or a `release_claim` never does.
  - This replaces `operator_requeued`.
  - It also removes the rule that a resumed held fix is held again as soon as it doesn't finish (`conclude_fix` with `previous_blocker`). After a `continue` the chain is ordinary again, bounded by the ceiling and the streak. If the prerequisite is still missing, the worker writes `BLOCKED.md` again.
- **The workspace sweep keeps what a checkpoint references.** Today it keeps the tree of every chain with a running or suspended attempt (`chain_status`). In the new model it keeps the trees of chains with a running attempt or referenced by a `checkpoint_job_id`.

```mermaid
sequenceDiagram
  participant S as Scheduler
  participant W as finding_work
  participant H as work_holds
  participant O as Operator
  S->>W: attempts 1-4 pause, checkpoint = job 4
  S->>W: ceiling reached: failure 3 of 3
  S->>H: insert hold gave_up (checkpoint kept)
  O->>H: release continue, released_at = t
  S->>W: resume job 4 to job 5, ceiling counts from t
  S->>W: job 5 ends paused, checkpoint = job 5
```

---

## 9. Holds

### 9.1 Reasons

| Reason | Set when | Detail | Resume prompt |
|---|---|---|---|
| `prerequisite` | the worker wrote `BLOCKED.md` (any kind) | the report | "Re-evaluate the prerequisite …" (today's held-fix prompt, generalised per kind) |
| `stuck` | the third identical failure | failure key + output tail | "Earlier attempts kept failing with: … Continue and finish." |
| `gave_up` | the ceiling was reached and that failure completed the streak | the ceiling's reason (`4 attempts, the limit`, …) | as `stuck` |

The reason is an enum column. The prompt is chosen from it, not from the text.

`BLOCKED.md` becomes a generic worker report. The playbooks for recheck, engage, harvest and harvest-closed get a section on when to write it: for recheck, verifying needs something unavailable; for engage, a reviewer asks for a maintainer decision; for harvest, the PR can't be classified without a human.

### 9.2 Lifecycle

```mermaid
stateDiagram-v2
  direction LR
  [*] --> Active: hold inserted (stuck, gave_up, prerequisite)
  Active --> Released: operator action
  Active --> Released: finding leaves the tier
  Released --> [*]
  note right of Active
    at most one per (finding, kind),
    enforced by a unique partial index
  end note
```

### 9.3 Release actions

| Action | recheck | fix | engage | harvest | Effect |
|---|---|---|---|---|---|
| `continue` | yes | yes | yes | yes | resume from the checkpoint; ceiling counts from now. Only offered when a checkpoint exists. |
| `restart` | yes | yes | yes | yes | drop the checkpoint, start fresh |
| `reject` / `wontfix` | yes | yes | – | – | the verdict, as today; the finding leaves the tier |
| `classify` | – | – | – | closed PR | the operator classifies the closed PR by hand; the work item concludes |
| `mark_harvested` | – | – | – | merged PR | the PR is marked harvested without a review (§5.6, G7); the work item concludes |
| `dismiss` | – | – | yes | – | mark the current attention handled, as a no-op engage's answer would: the PR's newest feedback and its current problems are answered, and the attention is settled from that |

Every release appends a `release` event with the actor and action, and fills in `released_at`, `released_by` and `release_action` on the hold row.

### 9.4 Engage specifics

- **A held engage stays held when the PR gets new activity.** Only an operator releases it. The card shows "new activity since hold". The new activity may well be what the hold is waiting for, but whether it resolves the hold is the operator's call.
- **The attention flag isn't touched by holding.** The engage tier skips the work item while it is held. `Store::set_attention_aside` goes away.
- **A failed withdrawal counts as a failure.** Today it is marked engaged to stop the loop. With a streak it no longer needs that.

### 9.5 Invariants

1. At most one active hold per (finding, kind): a unique partial index.
2. A hold exists only for an active work item. When the finding leaves the tier, the same transaction releases the hold.
3. Every tier, `preview_next`, the sweep and the API use the same query to decide "held".
4. A hold never changes `findings.status`. In particular, `FindingStatus::Blocked` no longer exists: a held fix stays `queued`.

---

## 10. Storage

```sql
-- One row per (finding, kind) that has ever had state worth keeping.
CREATE TABLE finding_work (
  finding_id        INTEGER NOT NULL REFERENCES findings(id) ON DELETE CASCADE,
  kind              TEXT    NOT NULL,          -- recheck|fix|engage|harvest
  streak            INTEGER NOT NULL DEFAULT 0,
  last_failure      TEXT,                      -- the streak's key
  checkpoint_job_id INTEGER REFERENCES jobs(id),
  PRIMARY KEY (finding_id, kind)
);

CREATE TABLE work_holds (
  id             INTEGER PRIMARY KEY,
  finding_id     INTEGER NOT NULL REFERENCES findings(id) ON DELETE CASCADE,
  kind           TEXT    NOT NULL,             -- recheck|fix|engage|harvest
  reason         TEXT    NOT NULL,             -- prerequisite|stuck|gave_up
  detail         TEXT    NOT NULL,
  held_at        INTEGER NOT NULL,             -- epoch ms
  released_at    INTEGER,                      -- NULL = active
  released_by    TEXT,                         -- operator name | finding-left-tier
  release_action TEXT                          -- continue|restart|reject|wontfix|classify|mark_harvested|dismiss
);
CREATE UNIQUE INDEX work_holds_one_active
  ON work_holds(finding_id, kind) WHERE released_at IS NULL;

-- At most one running attempt per finding, and per (repo, kind) for
-- repo-level work (migration 031).
CREATE UNIQUE INDEX jobs_one_running_per_finding
  ON jobs(finding_id) WHERE state = 'running' AND finding_id IS NOT NULL;
CREATE UNIQUE INDEX jobs_one_running_per_repo_kind
  ON jobs(repo_id, kind) WHERE state = 'running' AND finding_id IS NULL;
```

- `kind`, `reason` and `release_action` are Rust enums mapped via `sqlx::Type`, as `CODING_STANDARDS.md` requires.
- The table is named `work_holds` rather than `holds`, so it doesn't read as related to `verdict_anchors.holds_while`.
- The checkpoint lives only on `finding_work`. A hold doesn't need its own copy.
- `events` stays the human-readable log, with two new kinds: `hold` and `release`. Nothing decides from it.

**Removed:**
- **Schema:** `FindingStatus::Blocked`, `jobs.blocker`, the streak columns on `findings` and `pr_state`.
- **Functions:** `block_fix_job`, `held_fix_blockers`, `job_blocker`, `prepare_blocked_resume`'s blocker read, `operator_requeued`, `streak_hold_report`/`is_streak_hold`, `set_attention_aside`, the re-hold of a resumed held chain in `conclude_fix`/`run_fix`.
- **Retirement writes** on finding-kind job rows: `give-up`, `workdir-gone` and `finding-moved` become events plus a cleared pointer.

---

## 11. Migration

1. **Create the tables.**
2. **Streaks:** copy `fix_attempts`/`last_fix_failure`, `recheck_attempts`/`last_recheck_failure` and `pr_state.harvest_attempts`/`last_harvest_failure` into `finding_work`.
3. **Checkpoints:** for each finding kind, the newest `suspended` job with no successor whose finding is in that kind's tier becomes `checkpoint_job_id`. Suspended rows outside a tier are already retired by the sweep today.
4. **Blocked fixes:** each `blocked` finding gets
   - a `work_holds` row: reason `stuck` if `jobs.blocker` starts with the streak report's prefix, otherwise `prerequisite`; detail = `jobs.blocker`; `held_at` = the job's `finished_at`;
   - its held job as checkpoint, but only if that job has a transcript (`session_file`). A hold whose job never ran a worker gets no checkpoint, e.g. a tree that couldn't be made (G9);
   - status `queued`.
5. **Drop** the old columns.

**Not migrated, because the data doesn't say:**
- a recheck that went back to `new` after a streak;
- a harvest given up and stamped `harvested_at`;
- an engage set aside.

All of them only left an event behind, and backfilling would mean parsing event text. They stay as they are. The operator can still requeue a recheck, or get a harvest redone by clearing `harvested_at` by hand.

---

## 12. Operator surface

- **API**
  - `FindingOut` gains `holds: [{ kind, reason, detail, held_at, actions }]`; `blocker` goes away.
  - `POST /api/hold/release { finding_id, kind, action, reason? }`: refuses with 409 if the hold is no longer active or the action isn't allowed for the kind.
  - `/api/verdict` no longer accepts `blocked` as a source status.
- **UI**
  - The Kanban "Blocked" section becomes a **Needs you** section, listing active holds of every kind.
  - Each card shows the reason, the detail, the time held, and the allowed actions.
  - Inbox findings are unaffected.
- **Summary preview:** a held work item is skipped by the preview, exactly as by the scheduler, through the same query.

---

## 13. Out of scope

- **Scheduler-level waits:** budget denial, the disk gate, the network gate, operator pause, overdrive.
- **PR sync errors** (an unparseable URL, a failed PR view): logged per cycle, as today.
- **Pausing a repo** (`repos.enabled = 0`) is not a hold and not a release action. It keeps its meaning: it takes the repo out of the rotation. Its paused chains still resume, and its holds stay.

---

## 14. What this resolves from #56

| Known issue | Resolved by |
|---|---|
| Ceiling bypass without an operator (restart, `release_claim`) | ceiling counted since the last release (§8) |
| Engage Err-arm loop without a bound | engage streak (§7) |
| A harvest given up on a merged PR can't be recovered | harvest hold with `continue`/`restart`/`mark_harvested` (§9.3) |
| Hold kind told apart by the `stuck:` prefix | `work_holds.reason` (§10) |
| Recheck stuck back in `new`, indistinguishable | recheck hold, finding stays `rechecking` |
| A given-up engage only visible as an event | engage hold (§9.4) |
| Runner errors and unmade trees not counted | counted as failures (§7, G9) |

---

## 15. Implementation plan

One PR, rebase-merged, each commit compiling and green on its own, with its own tests and docs:

1. **`finding_work`**: the table, the store API, and moving the streaks. No behaviour change.
2. **Checkpoint pointer**: resumable listing, `finding_pick`, `resume_plan` and the sweep go through `checkpoint_job_id`. Finding-kind job rows stop being rewritten after they finish.
3. **Dep-update guard** (§5.6, G2): first a test for today's case, a paused fix whose `queued` dep-update finding is handed over or refreshed; then both skip a finding whose fix work item has a checkpoint or an active hold.
4. **`work_holds`**: hold and release in one place; one shared "held" query for the tiers, the preview and the sweep; the ceiling counted since the last release. Migrate blocked fixes; remove `FindingStatus::Blocked`, `jobs.blocker`, `operator_requeued`, the prefix check and the re-hold rule.
5. **Recheck** onto holds: a streak and the ceiling end in a hold instead of `new`.
6. **Harvest** onto holds: a streak ends in a hold instead of stamping `harvested_at`. This step also counts the two harvest failures parked until holds exist: a failing default-branch fetch (#62) and a tree that can't be made (dropped from #5, kept on the branch `fix/harvest-unmade-tree`).
7. **Engage** onto holds: active while `pr_open` (G1); a streak for the Err arm, withdraw failures and pre-worker errors; remove `set_attention_aside`.
8. **Runner errors** counted as failures for every kind, and an unmade tree for engage. #5 already counts an unmade tree for fix and recheck; harvest comes with step 6.
9. **`BLOCKED.md` for every kind**: playbooks plus the report reading in each runner.
10. **API and UI**: `holds` on `FindingOut`, `POST /api/hold/release`, the Needs-you section, prompts chosen by reason.
11. **README, `BACKEND-CONTRACT.md`, and stale bits**: `JobState::Denied` (never written), "never a wallclock overrun", the `killed_reason` list in `types.rs`.

Steps 5–9 are independent of each other once step 4 is in. Step 3 fixes a problem that exists today; it can be split into its own PR if it turns out to be urgent.

---

## 16. Decisions on former open questions

- **Repo-level work** moved onto the same model in #92 (§17).
- **A released hold starts its streak from zero**, a `stuck` one and a repo's alike. After a `continue`, the work item gets the full `MAX_CONSECUTIVE_SAME_FAILURE` again before the next hold.
- **`mark_harvested` records only the release** (who, when, the action) on the hold row, no operator note.

---

## 17. Repo-level work

Hunt and the analysis scans (`test_gap`, `dep_update`, `refactor`, `modernization`, `standards`) run on the same machine as finding work (§6.2). Where this section is silent, the finding rules apply unchanged: §4 facts vs. state, §6.3 transitions, §7 failures, §8 checkpoints and the ceiling since the last release, §9 holds and their invariants.

### 17.1 Work items

- **A work item is `(repo, kind)`** for each repo job kind, plus **`(repo, sync)`**: the clone, the fetch and resolving `origin/<branch>`, which every kind needs first. A work item is active while its repo exists. Deleting a repo removes its work items and holds in the same transaction; reaping it drops whatever is left (`ON DELETE CASCADE`).
- **Storage.** `repo_work(repo_id, kind, streak, last_failure, checkpoint_job_id)`, the repo twin of `finding_work`; `sync` never has a checkpoint, which the schema enforces. Holds share `work_holds` with finding work: a hold's subject is exactly one of `finding_id` and `repo_id` (a `CHECK`), its `kind` is a finding kind for a finding and a repo kind or `sync` for a repo, and a second partial unique index keeps at most one active hold per repo and kind.
- **One "held" query.** `Store::held_work` reads both subjects in one statement. Every finding tier, the resume tier, the repo rotation and the summary preview read it. A held `sync` holds every kind of its repo: the rotation skips the repo, the "an uncloned repo is always a hunt" branch included, and the resume tier skips its paused chains.

### 17.2 Transitions and timestamps

- **Success** resets the streak and clears the checkpoint. It is a `Done` worker whose output file has no invalid entry, the hunt's "no new commits" skip, or a Renovate scan that settled. Only success moves `last_<kind>_at` (and, for a hunt, the watermark and the full re-hunt bookkeeping), exactly as before.
- **A failure does not move `last_<kind>_at`.** The work stays as stale as it was, so the rotation retries it at once, and three identical failures hold it instead of letting it use up its scan interval on each try. This replaces the cycle's starvation bump (`attempt_ended`) and its bump on an `Err`. Known limit, as in §7: only identical failures are bounded; failures that vary are retried every cycle. A scan that fails differently each time (`no findings file`, then `invalid entries`, then `worker killed`) is never held and keeps its repo at the head of the rotation.
- **Pause** points the checkpoint at the suspended attempt and leaves the timestamp alone.
- **A given-up chain** counts as the failure `resume chain gave up`, with the ceiling's reason as detail, where it used to bump the rotation timestamp. The same selection starts the work fresh; the third given-up chain in a row holds it as `gave_up`, with the chain as its checkpoint.
- **Empty outcomes are failures:** a `Done` worker with no output file, or with invalid entries. A partly invalid ingest still files its valid entries; it just doesn't count as a success.

### 17.3 Failure keys

Fixed strings; git's or the worker's words go into the detail.

| Work item | Keys |
|---|---|
| `(repo, sync)` | `clone failed`, `fetch failed`, `origin mismatch`, `branch not found`, `repo not cloned` |
| `(repo, kind)` | `workspace not created`, `runner error`, `worker <state>` (not `Done`, without a pause), `no findings file`, `invalid entries`, `resume chain gave up`, `cycle error` |

**`cycle error`** is any error the hunt's or scan's runner returns without counting it itself: a suppression list that cannot be read, a playbook that cannot be rendered (a missing template, an unfilled placeholder, a missing `CODING_STANDARDS.md`), a store read or write that fails. `run_cycle_inner` counts it on the candidate's `(repo, kind)` with the error as the detail, so the same error every cycle holds the work instead of keeping its repo at the head of the rotation. An error already counted (`repo_sync_failed`, `runner_failed`) is not counted a second time.

**The network's failures count nothing (#95, layer 2).** Before a clone or fetch failure, a runner error, or a worker that failed without metered work is counted, the repo's hosts are probed again (`Backend::first_unreachable(&work_hosts(..))`). If one is unreachable, nothing is counted, the checkpoint stays, and the cycle ends as `offline_denial`. `origin mismatch`, `branch not found` and `repo not cloned` are local and always count.

**Renovate (`dep_update` without a job).** The zero-token Renovate scan runs on the admin lane (`scheduler::dependency_scans`), for every enabled, cloned repo whose `dep_update` is due and is not held, running or checkpointed. A scan with invalid entries is the failure `invalid entries` and has no checkpoint. Renovate having no answer is not a failure: it records a request for the AI scan (`dep_scan_fallbacks`, migration 032), and the job lane's rotation offers the repo's `dep_update` only while that request is newer than its `last_dep_update_at`, so the two lanes never scan one repo's dependencies at once. The AI job is counted like any other, and its success answers the request. A store error while settling the scan is logged as an `error` event, and the duty's next run retries the scan. A clone that cannot be synced is skipped and counted nowhere: the job lane's own fetches keep the `sync` streak.

### 17.4 Holds and releases

| Action | a kind | `sync` |
|---|---|---|
| `continue` | yes, with a checkpoint | – |
| `restart` | yes: drop the checkpoint, start fresh | yes: retry now |

Releasing resets the streak to zero, and the give-up ceiling counts from the release. The first attempt after a `continue` resumes with a prompt chosen by the hold's reason. A held hunt leaves a full re-hunt request pending, and while the hunt is held the request is not urgent: it neither starts the hunt nor puts the repo ahead of the rotation for its other kinds. Only a successful full-scope hunt settles it.

### 17.5 Operator surface

- **API.** The summary's `repos` and `/api/repos` carry each repo's `holds` (`kind`, `reason`, `detail`, `held_at`, `actions`) and `failing` (the current streak of each failing kind, `kind`, `streak`, `last_failure`). `POST /api/hold/release` takes `{ repo_id, kind, action }` as well as a finding's `{ finding_id, … }`, exactly one of the two.
- **Repos page.** Each repo shows its `sync` hold and held kinds with the reason, the detail, the time and the allowed actions, and the streak of each kind that is failing.
- **Needs you (Kanban).** Lists repo holds alongside finding holds.

### 17.6 Removed

The repo arm of `list_resumable_jobs`, `create_job`'s `superseded` retirement, the `finding_id IS NULL` arms of `chain_status` and `live_jobs`, `retire_suspended_job`/`retire_job` and their callers in `end_checkpoint` and `record_job`, `resume_plan`'s `set_last_kind_at`, and `run_cycle_inner`'s bumps. A repo runner's `backend.run` error closes its job instead of leaving it `running`. No finished job row is rewritten for any kind.
