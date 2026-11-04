# Future — project path, the Rust refactor, and open design questions

Living document. Captures the project's direction, the 2026-09-06
architecture decisions, and ideas/trade-offs too big for a single session.
The big refactor (§3–§6) starts once the plan here is judged good, not
before. Smaller items (§8) proceed independently when they don't conflict
with the port.

---

## 1. Path of the project

Where we came from, where we are, where we're going. Each phase has an exit
criterion; we don't start a phase before the prior one's criterion is met.

### Phase 0 — Experiments (done)
Validated the core economics and mechanics: diff-focused hunting finds real,
human-triage-surviving bugs (exp1: 5/5); token costs are measurable and
cappable externally (exp1b: ~60k/fix, ~150k/hunt, ~32k session floor); window
state is readable from omp's local mirror (exp2). Evidence: `../EXPERIMENT-*.md`.

### Phase 1 — Single-user hunter v1 (superseded by Phase 2)
Python 3.14 stdlib-only daemon: scheduler + `Backend` facade (budget gate +
externally-metered `omp -p` workers, omp specifics confined to
`backends/omp_scavenge/`) + SQLite + triage UI on :8377. 317 tests green as
of 2026-09-06. Runs as a systemd user service. It works; it earned the next
phase. The Rust port has since taken over the service; this entry stands
as the record of what it replaced.

### Phase 2 — Rust port, feature parity (done)
Port the daemon to Rust, single-user, same behavior, same DB file, same
playbooks. Exit criterion: the Rust binary runs the full cycle (sync → engage
→ fix → hunt) against the live `data/hunter.db` for a week with no regression
vs the Python daemon; Python daemon is then retired.

Retired: `hunter/hunter/`, `hunter/tests/`, `schema.sql`,
`pyproject.toml`/`uv.lock`, the Python `justfile`, the `python` CI job, and
the Rust side's adoption of unstamped (Python-built) databases along with
`python_adoption_test`. `hunter/` itself stays — it is the daemon's root
(`config.json`, `data/`, `playbooks/`, `ui/`, `ui-svelte/`).

`test_schema_parity.py` went with it and is not replaced: it compared
Python's schema construction against Rust's migrations, and that oracle is
exactly what retirement discards. The property still worth holding is
asserted Rust-side and more strongly by
`hunter-rs/tests/migration_states_test.rs` — every reachable migration
state rolls forward to head, converging on identical schema with its rows
intact.

### Phase 3 — Service-ification
Auth (local accounts), tenancy (user ownership everywhere), per-user Claude
accounts + budgets, sandboxed workers, per-account scheduler queues. Exit
criterion: a second real user (not the operator) onboards themselves, connects
a repo + subscription, and gets a merged fix PR without operator intervention.

Milestones, each its own PR series, in order:
- **Feedback screening (PR #53).** engage/harvest act only on PR
  comments from people who can push and from configured bots.
- **M0 Sandbox.** bwrap around the worker (§5.3): it sees its worktree, a
  private omp home and the per-repo caches; no `HOME`, ssh agent, gh/glab
  config or database. Then **M0b**: network egress through an allowlisting
  proxy.
- **M1 Login.** Local accounts (argon2id; `hunter user add|passwd|disable`),
  30-day session cookie, every API route behind it, operator actions
  attributed in the event log. All users share one workspace.
- **M2 Data separation.** `repo_members(repo, user, role)`; every read and
  write scoped by membership (404, not 403, for others' ids). Sharing a repo
  is an owner's invite, never "add the same URL".
- **M3 Users' own repos.** Per-user forge credentials (encrypted at rest),
  explicit env for gh/glab/git instead of the operator's ambient logins,
  finding dedup per repo instead of global. Needs M0.
- **M4 Users' own LLM accounts.** One `Backend` per account with its own
  omp home; per-account scheduler loops under a global concurrency cap.
- **M5 Full separation.** Kernel-level isolation for untrusted code (gVisor
  or microVMs), GitHub App tokens, pooled funding of shared repos.

### Phase 4 — Open doors (invite-only)
Small invited group. Webhooks replace polling where volume warrants.
Observability, backups, abuse limits. Grow the job taxonomy (test-gap,
dep-update, refactor hunts already exist). Reviews of PRs hunter did not
open land earlier, single-user (§8.8); for other users they wait on M0.

---

## 2. Future user stories

The product target: a hosted web service. "Operator" = whoever runs the
instance; "user" = anyone with an account.

### Identity & onboarding
- As a user, I sign in with a local account the operator created for me
  (decided 2026-10-03: local username/password over GitHub OAuth -- no
  OAuth app, callback URL or client secret to run, and it works for GitLab
  users and offline).
- As a user, I connect repos via a GitHub App installation — the service gets
  scoped, revocable, per-repo credentials. Never a personal global token.
- As a user, I connect my own Claude subscription (BYO tokens — see §5.1) or
  bring my own API key (BYOK — see §8.3). I can see my usage and disconnect
  at any time.

### Hunting & triage (exists single-user; must survive tenancy)
- As a user, I register a repo; hunts run on idle budget from *my* account.
- As a user, I triage findings in the UI (queue / reject / wontfix / note),
  and my rejection reasons become my repo's suppression corpus.
- As a user, I can open a full finding detail page: complete evidence/plan,
  job history with token costs, PR timeline, recheck results, related
  findings (§8.5).
- As a user, I can trigger a manual cycle or an adversarial recheck.
- As a user, I tune per-repo and per-kind scan intervals ("hunt this repo
  every 6h, dep-update weekly") (§8.4).

### Fix & PR loop (exists single-user; must survive tenancy)
- As a user, queued findings become draft PRs authored via my GitHub App
  installation; PR feedback is noticed and engaged automatically.
- As a user, I see per-PR state (checks, reviews, conflicts) in the UI.

### PR reviews (new; design in §8.8)
- As a user, I pick an open PR hunter did not open and switch on **Watch**:
  it gets an automated review (evidence-first: comments must cite code,
  never vibes), and every later push gets a review of what it added, until
  the PR merges or closes.
- As a user, I choose at request time whether reviews post on their own.
- As a user, findings land as inline review comments; the threads hunter
  opened are resolved by hunter once a later round finds them addressed.

### Safety & trust
- As a user, my repo's code never executes with access to another tenant's
  data, tokens, or the host (sandboxed workers, §5.3).
- As a user, my Anthropic token is encrypted at rest and never leaves the
  worker boundary for another tenant's job.
- As an operator, a worker can never push to a default branch (existing
  invariant, kept infra-level).

### Operations
- As an operator/user, I see budget state, cost-per-finding/repo, burn-rate
  charts, job history, and failure modes (§8.2).
- As an operator, the whole service is one binary + one SQLite file + Caddy;
  backup = Litestream-style replication of the DB.

---

## 3. The decision: pivot to Rust

**Status:** decided 2026-09-06, planning here; build starts when this doc is
judged a good plan.

Not a bug-prevention fantasy — the recorded bug history (prompt-escaping,
GitHub timestamp skew, force-push semantics, budget policy) is
policy/integration bugs a type system doesn't catch. The pivot rests on four
arguments that are true anyway:

1. **Tenancy safety is type-shaped.** The Python core flows through
   `Row = dict[str, Any]` with stringly access (~80+ sites in scheduler.py
   alone). Multi-tenancy is exactly where that style fails: one forgotten
   `WHERE user_id = ?` is a cross-tenant leak, and dict-rows make it silent.
   sqlx compile-time-checked queries + a `UserId` newtype make it structural.
2. **AI-maintained codebases inherit the language's *median* corpus register.**
   This repo is AI-coded, and it defaulted to Python's median idiom (dict
   shuffling, `.get()` chains) — nobody chose that. Python's training corpus
   is huge, unfiltered, and self-reinforcing (bad Python runs, gets committed,
   becomes corpus). Rust's corpus was pre-filtered by the compiler. More
   important: the compiler is an in-loop verifier — AI mistakes bounce off
   `cargo check` in the same turn at near-zero cost instead of surfacing at
   runtime if a test happens to cover them. At hundreds of AI edits, you get
   the language's median output at scale; pick the language whose median is
   acceptable.
3. **The domain fits.** Finding statuses are a state machine begging for
   exhaustive `match`; the `Backend` protocol's `Outlook` (paired
   Granted/Denied verdicts) is a sum type already straining against Python;
   forge responses become serde structs; "run_cycle never raises" is what
   `Result` is. A long-running daemon holding other people's OAuth tokens is
   where one static binary, no runtime, memory-safe stops being aesthetics.
4. **Timing is now-or-never.** This is the smallest the codebase will ever
   be, and the webservice plan rewrites most of server.py regardless. Plus:
   side project with learning as an explicit goal, and the operator is
   productive in Rust.

**Costs, accepted knowingly:** worker plumbing (subprocess spawn, JSONL
tailing, SIGTERM trees) is more verbose; iteration on scheduling/prompt logic
is slower; borrow-checker friction in shared-state spots. Counterweights:
models hallucinate more in exotic Rust — stay in the best-represented corner
of the ecosystem (axum/tokio/sqlx/serde, boring mainstream crates only). And
the compiler catches type bugs, not policy bugs: playbook/policy correctness
still needs tests and review in any language.

**Rejected alternatives, for the record:**
- *Tighten Python instead* (strict pyright, dataclasses at boundaries): viable
  for a deadline work project, but it fights the corpus prior with per-session
  instructions, and instructions decay; compilers don't.
- *Go / TypeScript:* no argument beat Rust for this operator's goals; the
  concurrency needs are trivial in any of them.
- *Moving off SQLite:* rejected. Write rates are tiny; WAL serves hundreds of
  triage users; single-writer matches the scheduler model; migration is a
  file copy. Revisit only with real multi-node pressure (unlikely ever).

---

## 4. Target stack

| Layer | Choice | Notes |
|---|---|---|
| Language | Rust | see §3 |
| HTTP | axum + tower | sessions are a small table of our own (`sessions`), not tower-sessions: its SQLite store pins sqlx 0.8, and two sqlx versions cannot link one libsqlite3 |
| Async runtime | tokio | per-account queues are tasks, not threads |
| DB | SQLite via sqlx | same file, same schema lineage; compile-time-checked queries |
| Serialization | serde | typed structs for worker JSONL, forge responses, config |
| Auth | local accounts, argon2id (`argon2` crate) | operator-created via the CLI; no OAuth app to register |
| Repo access | GitHub App installation tokens | kills the global `gh` credential model |
| TLS / edge | Caddy reverse proxy | the binary stays plain HTTP on localhost |
| UI v1 | Svelte SPA against a clean JSON API | novelty budget goes to the backend; one unknown at a time |
| UI styling | hand-written CSS custom properties | Tailwind was planned with the Svelte rewrite (§8.1) but not adopted; the token system carried over as-is |
| UI later | Leptos experiment allowed | affordable *because* the JSON API boundary makes the UI swappable in a weekend |
| Sandbox | rootless podman per job | §5.3 |
| Backup | Litestream-style SQLite replication | Phase 4 |

**What ports unchanged (language-neutral assets):** the SQLite schema
(`schema.sql` lineage), `playbooks/*.md`, `config.json` shape, the omp worker
contract (headless `omp -p`, session-JSONL metering, SIGTERM at cap,
kill-safe incremental output), the systemd unit approach, and the 317 Python
tests — which are the behavioral spec, ported module-by-module alongside each
component.

**Web system, consolidated (2026-09-06).** axum is the decided web layer,
not a default. Rationale against the field:
- *actix-web:* mature/fast but middleware is its own world; axum middleware
  is plain `tower::Service`, so sessions/auth/rate-limit/tracing compose
  from one shared ecosystem. Performance is irrelevant at our request
  rates. tokio-team maintenance.
- *Rocket:* slower cadence, thinner corpus — fails the AI-median criterion
  (§3.2) that motivated the pivot.
- *Loco / Rails-likes:* magic + young + thin corpus; wrong on all three axes.
- *Leptos full-stack:* rejected for v1 (§3, §7.3).
- Deciding argument is §3.2 itself: axum+sqlx+serde is the best-represented
  corner of the Rust corpus — for an AI-maintained codebase that's a primary
  criterion, not a tiebreaker.

Supporting crates: `tower-http` (static files, compression, trace),
`argon2` + `sha2` + `getrandom` for accounts and session tokens (Phase 3),
`tracing`/`tracing-subscriber`, `rust-embed` later for
single-file deploy. The daemon shape survives unchanged: axum is a tower
service on tokio, so "UI server + scheduler loop in one process" becomes
"axum router + scheduler task in one binary" — same systemd unit model.

---

## 5. The hard problems (none are framework-shaped)

### 5.1 Funding model: BYO subscription
Each user connects their own Claude subscription; the service schedules
against *their* windows. Consequences:
- Per-user omp homes (`OMP_HOME=data/users/<id>/omp`) with injected OAuth
  credentials; per-user usage probing; per-user budget gates — i.e. one
  `Backend` instance per connected account.
- We store users' Anthropic OAuth tokens server-side → encrypt at rest,
  minimize exposure surface, audit access.
- **Flagged risk:** automating consumer subscriptions on users' behalf in a
  hosted product is ToS-gray with Anthropic. Decide consciously before
  inviting strangers; invite-only keeps this a group of consenting adults.
- Operator-funded alternative was considered and argued against: it inherits
  metering/fairness/abuse problems and breaks the per-account budget model.
- BYOK (API key, dollar budgets) is the complementary funding model and the
  second `Backend` implementation (§8.3).

### 5.2 Scheduler: parallelism, per-repo then per-account
Two layers, one design:
- **Single-account (portable today):** one job per repo concurrently instead
  of one global job. The per-repo sequential invariant stays (git worktree
  safety). Needs per-repo locking instead of the global `_cycle_lock`; the
  budget gate already accounts for in-flight jobs via
  `SpendLedger.running_estimate()`. The `Backend` protocol supports this —
  nothing in `decide()`/`run()` assumes single-threaded dispatch; only the
  daemon loop and the Store's job-state management assume one-at-a-time.
- **Multi-account (Phase 3):** one logical queue per connected Claude
  account, a global concurrency cap on top, `decide()` evaluated per-account.
  The one-job-at-a-time rule exists because parallel fan-out self-DoSed *one
  account's* 5h window (observed live) — the constraint is per-account, so N
  users run N concurrent workers safely.

In the Rust port, tokio tasks make both layers natural; design the scheduler
loop for per-repo tasks from the start, even if v1 caps global concurrency
at 1.

### 5.3 Sandboxing (mandatory before any second tenant)
Today a hunted repo's build script runs with the operator's full credentials.
Required shape:
- `Backend::run()` stays the single spawn seam; wrap the argv in rootless
  podman: mount in the job worktree + that user's omp home, nothing else.
- Session-JSONL metering currently reads the host `~/.omp/agent/sessions` —
  the container moves that path; the meter follows via volume mount or
  in-container path mapping.
- The real work is credential scoping, not container tech: per-repo GitHub App
  installation tokens instead of any ambient identity.
- Sandboxing is worth doing while still single-user (repo code currently sees
  operator credentials) — it is Phase 3's first item, not its last.

**Stepping stone (single-user, pre-container): bwrap.**  bubblewrap is
rootless, already available on most Linux systems, and needs zero infra.
The harness wraps the `Popen` call:
```
bwrap \
  --ro-bind /usr /usr --ro-bind /bin /bin --ro-bind /lib /lib \
  --ro-bind /etc /etc --symlink usr/lib64 /lib64 \
  --proc /proc --dev /dev --tmpfs /tmp \
  --bind data/envs/$SLUG /home/hunter \
  --bind $WORKTREE $WORKTREE \
  --setenv HOME /home/hunter \
  -- omp -p "..."
```
This gives the worker full access to the host's toolchain (gradle, cargo,
node, python — all in `/usr/bin`) while hiding `$HOME`, `~/.ssh`,
`~/.config/gh`, and other repos. Blast radius of a compromised worker =
one project's build cache. No container images, no per-repo Dockerfiles.

**Per-project persistent environments.**  Each repo gets a persistent home
directory (`data/envs/<repo-slug>/`) that persists across runs. Workers can
install deps, configure tools, prime caches — whatever the project needs.
First run is slow (bootstrapping), subsequent runs reuse the cached env.

```
data/envs/
  glosdalen/         ← persistent $HOME for glosdalen workers
    .gradle/
    .m2/
    .local/
  signalk-pose/      ← separate env, separate project
    .cargo/
    node_modules/
```

This solves the container tool-availability problem without fat base images:
the host's tools are bind-mounted read-only, the project's deps live in
its own persistent env. A worker can `npm install` on first run; next run
it's already there. To reset a broken env: `rm -rf data/envs/<slug>/`.

Environment setup can be:
1. **Automatic:** the existing playbooks already tell the worker to build
   and test. If deps are missing, the worker installs them. The env
   persists, so next run they're already there.
2. **Explicit:** per-repo setup script or a first-run playbook triggered
   when `data/envs/<slug>/` is empty.

Option 1 matches "add a repo URL and go" — no per-repo config needed.

**Credential exposure audit (current state):**
The worker subprocess inherits the full host environment. Specifically
exposed but NOT needed by the worker:
- `~/.ssh/*` — the scheduler pushes, not the worker
- `~/.config/gh/*` — the scheduler calls `gh`, not the worker
- `~/.omp/agent/*` — the harness reads session JSONLs externally
- Other repos' worktrees in `data/wt/` and `data/repos/`
- All env vars, full network, full filesystem

The bwrap approach hides all of these. The podman endgame adds restricted
egress (see the egress policy under tier 3 below) and per-repo credential
injection via GitHub App installation tokens.

**System-package installation under bwrap — the `/usr` tension.**
bwrap bind-mounts `/usr` read-only from the host, so the worker gets
the host's toolchains (JDK, Node, Python, Rust, etc.) for free. But
`apt install` / `dnf install` / `pacman -S` need root and a writable
`/usr` — neither available in a rootless bwrap sandbox.

Three tiers, in order of complexity:

1. **Don't (covers ~95% of repos).** Most projects don't need system
   packages installed — they need their own build tool's dependency
   manager (`npm install`, `pip install -r`, `./gradlew`, `cargo build`),
   which all install into the worktree or `$HOME` (userspace). The host
   already has the language runtimes; the project-level deps are what
   the persistent env caches. If a repo genuinely needs a system package
   the host doesn't have, install it on the host (single-user scenario)
   — this is the same thing you'd do for your own dev work.

2. **Per-project overlay (rootless overlayfs).** Layer a writable overlay
   on top of the read-only `/usr` via fuse-overlayfs. The worker sees a
   writable `/usr/bin` but writes actually go to
   `data/envs/<slug>/overlay/`. Persists across runs, per-project, host
   untouched. bwrap supports this with `--overlay-src /usr --overlay
   data/envs/<slug>/usr-overlay --tmp-overlay /usr`. More complex but
   gives full `apt install` capability without root or containers.
   Trade-off: the overlay accumulates drift from the host's base — an
   `apt upgrade` on the host doesn't propagate into existing overlays.

3. **Podman with persistent volumes (the endgame).** A rootless container
   with a base image providing the OS + system packages, and a persistent
   named volume for the project home. Workers can `apt install` freely
   inside the container — it's an ephemeral filesystem layer. The project
   home volume persists; the system-package layer is cheap to rebuild from
   the base image. This is heavier but is the multi-tenant endgame anyway.
   The base image can be thin (debian-slim + git + common runtimes) since
   the worker self-provisions the rest.

   Podman fits this well specifically because:
   - **Rootless by default** — runs as your user, no daemon, no privilege
     escalation. Matches bwrap's security posture.
   - **Volumes are just directories** — stored at
     `~/.local/share/containers/storage/volumes/<name>/_data/`. Inspectable,
     removable, backuppable. `podman volume rm hunter-env-glosdalen` resets
     a broken env.
   - **No registry needed** — `hunter-base` can be built locally from a
     Dockerfile or `podman import` from a tarball. No network dependency
     after initial build.

   Concrete shape:
   ```
   # One-time per repo
   podman volume create hunter-env-glosdalen

   # Every worker run
   podman run --rm \
     -v hunter-env-glosdalen:/home/hunter \
     -v $WORKTREE:$WORKTREE \
     --network=slirp4netns \
     hunter-base \
     omp -p "..."
   ```

   **Egress policy.** The worker cannot run with `--network=none`: `omp` is
   an LLM agent and must reach its API endpoint, and the persistent-env
   design above depends on the worker installing dependencies on first run
   (`npm install`, `apt install`, cache priming). What the worker must *not*
   have is forge write access — pushes are the scheduler's job, with scoped
   per-repo tokens (§5.3 opening). So the requirement is restricted egress,
   not no egress:
   - **allow:** the LLM endpoint, package registries (npm/PyPI/crates/Maven)
   - **deny:** the forge's write API, anything holding operator credentials

   Enforcement options, cheapest first: a filtering proxy the container is
   pointed at via `HTTPS_PROXY` with a host allowlist; a pasta/slirp4netns
   port-restricted config; or a full netns with nftables egress rules. An
   offline-only variant (`--network=none`) is viable only if every
   dependency is pre-provisioned into the volume beforehand, which
   contradicts the "worker installs what it needs" model above — pick one.

   The volume persists across `--rm` containers. `apt install`, global caches,
   tooling config all survive in `/home/hunter`. The container filesystem itself
   is ephemeral — a corrupted system state is just a re-pull from the base image
   while the project's deps stay cached.

   Shared caches are possible — content-addressed caches (gradle, cargo
   registry, pip wheels) can be mounted read-write across repos:
   ```
   -v hunter-cache-gradle:/home/hunter/.gradle
   -v hunter-cache-cargo:/home/hunter/.cargo/registry
   ```

   omp availability inside the container — options:
   1. Bind-mount the host binary: `-v $(which omp):/usr/bin/omp:ro`
   2. Install it in the base image
   3. Bind-mount `/usr` read-only (same as bwrap — keeps all host tools,
      container only adds writable overlay for `apt install`). Makes podman
      behave like "bwrap with egress control and system package support."

   Option 3 is appealing for single-user: no base image to maintain, host
   tool updates propagate instantly, and the container's value-add is purely
   isolation + writable system layer. Option 2 is cleaner for multi-tenant
   (the container is self-contained, portable, reproducible).

Recommendation: start with tier 1 (host tools + userspace deps). Move
individual repos to tier 3 (podman) when they genuinely need system
packages the host doesn't have or when multi-tenancy requires full
isolation. Tier 2 (overlay) is a niche middle ground — worth knowing
about but probably not worth the complexity if podman is the endgame.

### 5.4 Harness coupling (the most fragile dependency, in any language)
Session-JSONL format, `agent.db:usage_history` schema, session-file reuse
behavior — a harness update breaks metering and the budget gate silently, and
the failure direction is overspend. The Python side already confines this:
the `Backend` protocol (`backend.py`: `decide()` / `run()` / `keep_fresh()`
+ `SpendLedger`) with all Anthropic/omp vocabulary inside
`backends/omp_scavenge/` (facade + capacity). The port keeps that boundary:
- `Backend` becomes a Rust trait; `omp_scavenge` its first impl. A future
  switch (e.g. Claude Agent SDK with programmatic usage callbacks) or a BYOK
  backend (§8.3) is a module swap.
- Pin the omp version the service uses.
- Startup canary: parse a known-good ledger fixture + assert the
  `usage_history` schema; refuse to run blind.

---

## 6. Port plan (Phase 2, in rounds)

Each round ported modules *and* their test suites; tests are the spec.
The Rust binary and the Python daemon ran side-by-side against the live DB
during the transition (SQLite WAL is multi-process), and cutover was
per-component. That is finished: every round below is DONE, the Python
daemon was stopped, and the Rust binary owns :8377 and the database (see
the Round 3 entry). The plan is kept as the record of how it was done.

### Round 1 — validate the web system, read-only — **DONE 2026-09-06**

Front-loads the layer with the most operator-unknowns (axum stack, sqlx vs
the real schema, side-by-side operation) at zero risk: no writes, the
Python daemon keeps scheduling.

1. Scaffold `hunter-rs/` (layout decided, §7.7): single bin crate, modules
   mirroring the Python layout (`types`, `store`, `backend`,
   `backends/omp_scavenge`, `runner`, `forge`, `scheduler`, `server`).
   No workspace split until earned.
2. sqlx against the live `data/hunter.db`; offline query metadata
   (`cargo sqlx prepare`) so builds never need a live DB.
3. Port **types** + **store read paths** with tests.
4. axum serving the existing read-only JSON API + the current vanilla-TS
   UI on :8378.

**Exit criterion — MET 2026-09-06:** the vanilla UI loads fully from the
Rust binary (:8378) against the live DB beside the running Python daemon:
1103 finding cards, all views live, zod passes, zero console errors;
scripts/parity.sh 8/8 endpoints PASS vs :8377. 9 Rust tests green, clippy
clean. Notes: dev.db (schema-only, via Python Store init) backs sqlx
compile-time checks — `just dev-db` regenerates; `cargo sqlx prepare`
offline metadata deferred until CI exists. Round-1 stubs in /api/summary
(backend_status_html, cycle_running=false, next_candidate=null,
activity_status variants Working/Paused/Ready unreachable) are marked
with round-2 comments in server.rs. Accepted deviations from Python:
param validation returns 400 not 500; last_cycle uses a direct SQL
lookup, not a 500-event window; Store is read-only at the connection
level (cannot write the live DB).

### Round 2 — writes + budget brain — **DONE 2026-09-13**
Store write paths (triage verdicts, notes, repo registration), all 9 POST
endpoints, `Backend` trait + omp_scavenge capacity math (decide/keep_fresh/
status_html), scheduler pick_next + anticipated_tokens. Cycle/override
forwarded to Python daemon via reqwest shim (temporary, round 3 deletes).

**Exit criterion — MET 2026-09-13:** 123 tests green (28 capacity, 45
facade, 19 post, 11 store-write, 11 scheduler, 4 server, 5 store-read);
parity 8/8 vs Python on live DB; real backend status bars (5h/7d windows,
ramp markers, availability), real pick_next candidate with budget decision,
full POST flow (add/delete repo round-tripped, cycle forwarding 409-busy
from running Python daemon). Accepted deviations: UTC in notes timestamps
(Python used local), UTC in status reset time (flagged for round 3),
param validation returns 400 not 500, non-object JSON body returns 400
not leaked 500.

### Round 3 — the daemon — **DONE 2026-09-14**
Runner (harness: subprocess spawn, JSONL metering, SIGTERM at cap),
forge (GitHub/GitLab via gh/glab CLI), playbooks (template rendering),
ingest (findings JSON validation + fingerprint dedup), all scheduler
executors (run_hunt/recheck/fix/engage/harvest/sync_prs + generic
_run_analysis_job for test_gap/dep_update/refactor/modernization),
daemon loop (axum + scheduler + usage prober as tokio tasks, lockfile,
reconcile, compute_sleep, describe_cycle), CLI `daemon` subcommand.

**Exit criterion — INITIAL PASS 2026-09-14:** Python daemon stopped,
Rust daemon started on :8377, ran a full cycle (PR sync across 77 repos
+ refactor job on harbour-sim, worker metered + killed at cap, ingest
ran), UI loaded 1109 cards with zero console errors, real backend
status bars. 173 tests green, zero todo!()s in 8069 lines of Rust.
One-week parity run starts now (Phase 2 exit criterion = §1).

---

## 6b. Svelte UI rewrite — **DONE 2026-09-14**

Migration 2 per §4/§8.1: vanilla-TS SPA (1560 lines, 1 file) replaced by
Svelte 5 (runes) + Vite. Tailwind was *not* adopted — styling stayed
hand-written CSS with the existing custom-property token system
(`hunter/ui-svelte/src/app.css`). Source at `hunter/ui-svelte/`,
production build outputs to `hunter/ui/` (where the Rust binary serves it).

Components: App shell with sidebar nav + hash routing, StatusPage (backend
window bars via {@html}, exhaustive activity_status switch, Run Cycle
button), FindingCard (severity/confidence/category/status badges, verdict/
recheck/override actions, expandable detail panel), FilterBar (dropdown
checkbox groups for repo/type/class/severity, confidence slider, sort
selector), InboxPage/KanbanPage/AllFindingsPage (three views of the same
finding data with independent filters), ReposPage (list + add/remove/toggle
+ notes), StatsPage (totals + by-kind + by-finding tables), LogPage
(events + jobs tables with kind/state badges).

Reactive API store (HunterStore class with $state runes, 5s polling,
lazy detail/notes fetching). TS types manually maintained (typeshare was
tried but rejected — can't handle i64, serde flatten, internally-tagged
enums). Format helpers shared across components.

Build: 90KB JS + 25KB CSS gzipped to 29KB + 5KB. All 7 views render
with live data, zero console errors, dark theme matching the original.

### Module notes (dependency order)

1. **types** → domain structs: `FindingStatus` etc. as enums with typed
   transitions; `Config` via serde. Kill `Row` — no dict-shaped core.
2. **store** → sqlx against the existing schema; add nullable `user_id`
   ownership columns (backfilled to operator) *during* the port — tenancy
   retrofits get expensive later. `SpendLedger` impl comes with it (the
   thread-local dance in `ThreadLocalLedger` dissolves under sqlx's pool).
3. **backend protocol + omp_scavenge** → `Backend` trait (`decide`/`run`/
   `keep_fresh`), `Outlook`/`Granted`/`Denied` as enums, capacity math
   (ramps, unaccounted-token tracking) ported test-first — it's pure logic
   and spec-rich; proves the pattern. Decisions cross as *data* (`Outlook`),
   diagnostics as *presentation*: §8.1's open question closed the other way —
   `status()` stayed an HTML fragment (`Backend::status_html`), because typing
   the payload would have put one backend's window vocabulary into the shared
   contract. The coupling is paid for with a byte-exact spec instead.
4. **runner** → the subtle one: spawn into a private `--session-dir` (omp's
   `autoResume` otherwise resumes the cwd's previous session and re-caches
   it), live tailing, SIGTERM-at-cap, kill-safety.
5. **forge** → serde-typed GitHub/GitLab responses; GitHub App tokens replace
   `gh` CLI ambient auth (may land as a Phase 3 follow-up; keep the seam).
6. **scheduler** → port = refactor: the copy-pasted `run_*` skeletons
   (hunt/recheck/test-gap/dep-update/refactor share one ~100-line shape)
   become one generic job executor + per-kind descriptor (playbook builder,
   `JobClass`, model key, output parser). Do NOT transliterate the
   duplication. New job kinds (PR review) become a descriptor entry, not a
   copy. Structure the loop for per-repo parallelism (§5.2) even if capped
   at 1.
7. **server** → axum from scratch, but *first* in the round order (see
   round 1): read-only API parity validates the stack early; auth redesign
   comes in Phase 3. UI initially unchanged (current vanilla-TS SPA talks
   to the same JSON endpoints); Svelte rewrite follows as its own
   step.

Invariants that MUST survive the port (all currently enforced and tested):
- Workers never push to a repo's default branch (infra guard, not prompt-only).
- PR-branch history rewrites before merge are normal workflow, not violations.
- Plain `--force` on raw-URL pushes (`--force-with-lease` structurally fails
  without a named remote — twice-learned). The engage push is the exception:
  it leases with an explicit `<ref>:<sha>` (the tree's pinned PR head), which
  reads no tracking ref, so it cannot delete commits pushed while it ran.
- Fix-queue drains before new hunts; PR feedback sync is never delayed by
  token-budget backoff.
- run_cycle survives anything; the daemon loop never dies to a job error.
- Kill-safe incremental findings output (hunt playbook contract).
- 60s watermark buffer on GitHub timestamp comparisons (±1.6s observed skew).
- Prompt building escapes ALL interpolated content (template injection).
- `Outlook` invariant: prioritized is at least as permissive as normal.

---

## 7. Open decisions

| # | Decision | Status |
|---|---|---|
| 1 | BYO subscription as the funding model | **Leaning yes** (argued §5.1); confirm before Phase 3 code |
| 2 | ToS-gray of hosting others' Anthropic tokens | Open; invite-only mitigates; revisit before strangers |
| 3 | Svelte vs Leptos for the UI rewrite | **Svelte decided, reinforced 2026-09-06.** Leptos author declared the framework feature-complete and "lightly maintained going forward" (leptos-rs/leptos#4707, May 2026; maintainers sought, none onboarded as of July). Svelte wins on iteration speed (HMR vs 30–90s rebuilds), corpus (§3.2 cuts for Svelte here — Leptos corpus is thin and split across 0.5/0.6/0.7 idioms), governance (paid Vercel team), and architectural fit (Leptos server functions would dissolve the JSON API boundary). Leptos's one big win — end-to-end shared Rust types — is ~80% recoverable by generating TS types from the serde structs (typeshare/schemars). Experiment downgraded: revisit only if governance improves. |
| 4 | `gh` CLI → GitHub App: during port (step 5) or Phase 3 | Open; seam kept either way |
| 5 | Webhooks vs polling threshold | Defer to Phase 4; polling fine at current repo counts |
| 6 | Keep omp vs Claude Agent SDK for workers | Keep omp; revisit only if the coupling (§5.4) keeps hurting — trait makes it cheap |
| 7 | Repo layout | **Decided:** `hunter-rs/` beside `hunter/`, single bin crate, no workspace until earned (§6 round 1) |
| 8 | Tailwind on the *current* UI before the port | **No** — superseded; Tailwind was to arrive with the Svelte rewrite (§8.1), but in the event was never adopted |

---

## 8. Nearer-term items (pre-existing living-doc entries, reconciled with the pivot)

### 8.1 UI: Tailwind CSS + `status() -> data, not HTML`
**Status:** closed. Both halves were decided against what this section
proposed, so it is kept as the record of why.

**Tailwind: dropped.** The plan was to migrate the hand-rolled ~300-line
CSS to Tailwind, and then to fold that into the Svelte rewrite rather than
pay the migration twice. The rewrite (§6b) happened; Tailwind did not. The
existing token system (spacing/type scale, radii, shadows) already carried
the visual identity, and porting it into a framework config bought nothing
a rewrite was not already delivering. `hunter/ui-svelte` is plain CSS.

**`status() -> data`: not adopted either.** The argument for it stands —
an HTML fragment couples the backend to CSS class names and cannot be
validated at the schema boundary beyond `z.string()`. It lost to a
stronger constraint: the backend, not the core, owns what its status
means, and the set of window dimensions a backend reports is its own
vocabulary. Typing that at the boundary would have put one backend's
shape into the shared contract. So the fragment stays, and the coupling is
handled by *specifying* it: BACKEND-CONTRACT §2.3 pins the markup to the
character, and `status_html_test` snapshots nine states against it. The UI
injects it with `{@html}` and owns the stylesheet those class names
resolve against.

### 8.1b Serve the UI bundle from the binary
**Status:** idea; do it after the Rust port merges.

The bundle used to be committed, which meant a generated artefact in the
tree, a rebuilt hash in every deploy commit, and a permanent conflict for
any branch that tried to stop tracking it. That is fixed: `hunter/ui/` is
generated and ignored.

What the fix leaves behind is an ordering trap. The daemon reads
`ui_dir/index.html` from disk per request (`static_files` in
hunter-rs/src/server.rs), so a restart
without a build serves a working API behind a 404 page — which is exactly
what happened the moment the untracking commit landed on this branch. The
mitigation is `just build`, which builds both halves so neither can be
forgotten; restarting stays a separate, explicit `systemctl --user restart
hunter.service`. That is a convention, not a guarantee: restarting without
building first still breaks the UI, and so does a fresh clone.

Embedding removes the class rather than documenting it. `include_dir!`
(or `rust-embed`) compiles `hunter/ui/` into the binary, `cargo build`
becomes the single build step, `ui_dir` and its path-traversal guard
(also in `static_files`) disappear, and there is no directory to be missing.

Costs, none of them blocking:
- `build.rs` has to run `vite build` first, or the bundle has to be built
  before `cargo build` — trading a runtime ordering trap for a build-time
  one, except this one fails loudly at compile time.
- Rebuilding the binary to change a stylesheet. `hunter-rs` builds in
  ~30 s, and the UI is not iterated on in production.
- Dev ergonomics: keep `vite dev` with its API proxy for the edit loop, as
  now; embedding concerns the shipped artefact only.

Blocked on nothing except merge order: doing it before the port lands
means rebasing it under every review round.

### 8.2 Scheduler: job-level cost tracking and reporting
**Status:** idea; lands as a UI story in Phase 3/4 (§2 Operations).

`anticipated_tokens` already tracks historical cost distributions per kind.
Surface it: cost-per-finding, cost-per-repo, burn-rate charts. Data already
in the jobs table (`tokens_new`, `usage_delta`); needs a `/api/costs`
endpoint + a Stats sub-panel. Related: the `estimate_capacity` calibration
(p75 of token/fraction ratios) currently only feeds the "~Nk tok avail"
display; it could replace the hardcoded `200k ≈ 10%` conversion in the
facade, at least for display.

### 8.3 Backend: BYOK / bring-your-own-key
**Status:** open; the `Backend` protocol enables it; becomes a funding-model
pillar in Phase 3 (§5.1).

Most likely second backend: same omp harness (spawn/meter/kill JSONL
pattern), accounting via direct Anthropic API key (usage from the API, not
agent.db) — or OpenRouter/other provider — with a dollar-budget policy
(daily/monthly cap) instead of scavenging ramps. The protocol's genericity
is untested by a second implementation; a BYOK spike validates or breaks the
interface at exactly the joints the Rust trait must get right:
- `decide()` — does `Outlook` generalize beyond ramp-based pacing?
- `run()` — does `JobClass` (HUNT/FIX) make sense for a dollar backend?
- `keep_fresh()` — does it mean anything without probe staleness?
- `SpendLedger` — are 7 methods too many? Too few?

Worth doing as a spike *during* port step 3 (even a stub impl), so the trait
is shaped by two consumers before it hardens.

### 8.4 Scheduler: per-repo and per-kind scan intervals
**Status:** `scan_interval_days` done; extension idea.

Per-repo overrides in the repos table ("scan this repo every 6h, that one
weekly"; config default as floor). Per-kind intervals generalize the existing
`modernization_interval_days` into `scan_intervals: {hunt: 1, dep_update: 7,
...}`. Cheap to include in the Rust store/scheduler port; becomes a user
story in Phase 3 (§2).

### 8.5 UI: finding detail page
**Status:** idea; folded into the Svelte rewrite scope (§2 triage stories).

Full-page detail view: complete evidence/plan text, job history with token
costs, PR timeline, recheck results, related findings in the same
file/class. Data is already served (or trivially servable) by
`/api/finding/<id>`; it's purely a UI page. Don't build it twice — skip on
the vanilla UI, include in the Svelte rewrite.

### 8.6 Process: multi-repo parallelism
**Status:** absorbed into §5.2 (single-account layer). The prior analysis
(per-repo locking, `running_estimate` already in-flight-aware, protocol
already dispatch-agnostic) carries over verbatim as the design basis.

### 8.7 Tooling: frontend lint + SQL lint
**Status:** frontend lint done; SQL lint still an idea.

CI runs fmt, clippy `-D warnings`, `check-sql.sh` and nextest for
hunter-rs; and ESLint, `npm run check` (svelte-check), vitest and a Vite
build for the Svelte frontend. `check-sql.sh` enforces the Rust store's SQL
encapsulation (compile-time-checked queries only, no QueryBuilder); it does
not lint SQL text.

- **ESLint for the frontend** — done. It was scoped to the hand-written
  `ui/src/app.ts`, which the Svelte UI replaced; the Svelte sources are
  linted in the frontend CI job instead.

- **SQL linting for `schema.sql` and the query builders.** Review tooling
  repeatedly reports `python-fstring-execute` on `store.py` — SQL assembled
  by f-string and handed to `execute()`. Every current instance is safe: the
  interpolated parts are column names and `?` placeholder runs drawn from
  internal allowlists (`_JOB_COLUMNS`, the migration's `all_cols` list), never
  caller data, and values are always parameterized. But "safe by convention,
  re-litigated every review" is the wrong steady state. Options: sqlfluff for
  `schema.sql` style/dialect, plus either a narrow suppression with a comment
  documenting the identifier-allowlist invariant, or a small typed helper
  that makes the invariant structural (identifiers only from an enum) so the
  linter is satisfied by construction.

It blocks nothing; it would remove recurring review noise and a class of
bug the current gates cannot see.

### 8.8 PR reviews: Watch on PRs hunter did not open
**Status:** designed 2026-11-04; slices 1–2 built (manual request, Watch,
review rounds, results in the UI; inline COMMENT posting with per-review
auto-post or per-round approval). Not yet: thread replies and resolution
(slice 3), issues still open at merge becoming findings (slice 4).

Reviews are enabled by hand, per PR, never by repo-wide discovery: the
open PRs that are not hunter's own were, when designed, the operator's
own PRs, dependabot's, and one outside contributor's.

**Trust.** The reviewer is a normal worker with every tool: it may build
and run targeted tests, so it runs the PR's code with the operator's
credentials. Until M0/M0b there is no read-only tier: a PR whose code is
not trusted is not reviewed at all, because posting is only one of the
ways a session that reads attacker text can leak secrets (file reads, URL
fetches). Trusted are the PR's author, whom the manual request vouches
for, plus the set feedback screening already trusts (`github_can_push`,
`ReviewBots`) -- one definition, not a second one. GitHub answers who can
push only to a login that can, so hunter's must be able to push to a repo
it reviews, as to every repo it tracks.
- Requesting a review of a PR whose author cannot push asks to confirm
  ("this runs their code with your credentials"); the vouch is recorded
  with who gave it.
- A fork PR's branch can be pushed by the fork's own collaborators (and by
  maintainers when edits are allowed), and no API says who pushed a head;
  commit authorship is forgeable. So the code a round runs is the fork's,
  decided about by its owner: the fork's owner, as the author, must be
  able to push or be vouched for, and vouching for them vouches for their
  fork. The fork need not be the author's (a maintainer may open a PR
  from a stranger's fork), and it can change hands; a change stops the
  review until a request decides about the new owner. The request UI
  names whom a vouch is for.
- What a later round is shown from the thread (replies, rebuttals) is
  screened to the same trusted set; a stranger's comment never reaches a
  full-trust session.
- Single-user only: once other users exist (M2), a user's vouch would run
  a stranger's code on the operator's host, so non-pusher reviews for
  them wait on M0.

**Request and scheduling.** A Review action on a repo's open PRs (listed
when the view opens, not every cycle) or a pasted PR URL
(`Forge::parse_pr_url`) creates the watch. Any registered repo, enabled or
not: disabled means "do not scan". A requested review ranks beside engage
(tier 2): a human is waiting, but not ahead of hunter's own PRs with
feedback. Its jobs are a third arm, `JobKind::Pr(PrJobKind::Review)`;
`pr_state` and the finding kinds presume a finding, and a synthetic one
would be worse than a new arm.

**Watch.** Each round fetches `refs/pull/N/head` (GitLab
`refs/merge-requests/N/head`) into the repo's clone, so forks need no
access, and reviews it only once it names the head the forge reported:
the forge updates that ref some time after a push, and a fetch that still
finds another commit fails, to be retried. The first round reviews from the merge-base; later rounds review
`last_reviewed..head` with the earlier issues and the screened replies,
under `review.md`'s rebuttal rule; a force-push that drops the last
reviewed head reviews the whole PR again. A new head is reviewed once it
has been stable for ~10 minutes, so a burst of pushes costs one round.
The watch ends when the PR merges; a closed PR's review ends until the PR
is reopened and the review requested again, and one whose PR turns out to
be hunter's own ends for good. A blocking issue still open
at merge becomes a `new` finding, which the hunt's novelty check then
recognises when it reaches the merged commits.

**Posting.** Auto-post is chosen per request; otherwise a round's review
waits in the UI for approval. Hunter posts under the operator's login, so
every review is `COMMENT` (GitHub refuses approve/request-changes on your
own PR, and it must not read as the operator's verdict elsewhere) and its
body opens with an automated-review header. The posts, like the refresh
of every open review's PR and threads, are the admin lane's PR review
duty, beside the PR sync and never inside its pass: the engages held
for a sync pass, and the job lane's wake for a PR's end, must not wait
for review traffic. Approving a round wakes the duty.
- Inline, one batch per round. GitHub: one `POST .../pulls/N/reviews` with
  `commit_id` = the reviewed head and `comments[{path, line, side,
  start_line?}]`. A line outside the diff's hunks fails the whole request
  (422), so every issue's location is checked against the diff first and
  the ones outside it go into the review body. An issue on removed code is
  `side: LEFT`, so the reviewer's output gains a side (or old line).
  GitLab: `position{base_sha, start_sha, head_sha, old/new path and line}`
  from the MR's `diff_refs`, posted as draft notes and published one by one
  (`bulk_publish` would take the operator's own drafts along)
  (verify against the instance's API version). `GlPosition` /
  `CodeLocation` already map positions for reading; posting is the inverse.
- Only new issues are posted. Each posted issue keeps the forge id of its
  inline comment, read back after the post, by which its thread is found
  on every read; one a complete read no longer finds was deleted and is
  recorded gone. A round that finds nothing new posts nothing.
- A thread hunter opened whose issue a later round no longer raises (or
  whose rebuttal it accepts) gets a reply naming the commit and is
  resolved by hunter (GitHub `resolveReviewThread`, GitLab discussion
  `resolved=true`). Threads others opened are never resolved; a thread a
  human reopened is live again.

**Data.** `pr_reviews(repo_id, pr_number, watch, auto_post, vouched_author,
vouched_by, last_reviewed_sha, head_stable_since, state)` and
`pr_review_issues(review_id, fingerprint, file, line, side, blocking,
thread_ref, status: open|addressed|rebutted)`.

**Order of work.** (1) request + watch + review job, reviews visible in the
UI; (2) inline posting and auto-post; (3) incremental rounds, replies and
thread resolution; (4) unresolved issues at merge become findings; then a
dependabot playbook (changelog and breaking changes against our usage,
then build) -- the code-review prompt does not fit a version bump.
