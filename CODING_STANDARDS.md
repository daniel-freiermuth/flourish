# Coding Standards

Authoritative principles for all projects in this workspace. The union
of conventions established across logcrab, recentip, signalk-chart-rs,
and hunter-rs — proven by production use, not theoretical preference.

## Values

**Correctness → Maintainability → Boringness → Defensiveness**

- **Correctness first.** Code that produces wrong results is worse than
  code that crashes. Verify behavior, not just compilation.
- **Maintainability second.** The next person (or AI) reading this code
  six months from now matters more than saving a line today.
- **Boringness third.** Prefer the well-understood, widely-used approach.
  Novel solutions need to justify their novelty.
- **Fail early, fail loudly.** Never silently degrade. If a precondition
  is missing, refuse the operation with a clear error — don't produce
  garbage output that burns resources downstream. An error that surfaces
  at the point of failure is cheaper than one that surfaces three layers
  later as mysterious wrong behavior.

Additional:
- **Full agency.** Own the code. Delete what isn't pulling its weight.
  Refuse unnecessary abstractions.
- **Do one job, do it well.** Each module, function, and type has a
  clear single responsibility.
- **PoC-first.** Prove the pipeline works end-to-end, then build
  incrementally. Never design in isolation.

---

## Safety

### No unsafe code
`unsafe_code = "forbid"` (or at minimum `"deny"`). Use safe wrappers
(`nix`, `socket2`, and the standard library's own `File::try_lock` for
file locks) instead of raw FFI. Zero exceptions in
application code; document any library exception with a safety proof.

### No panics in production code
```toml
[lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"
```
Tests may `#![allow(...)]` these. Production code propagates errors
via `Result`, uses `unwrap_or`/`unwrap_or_default` for infallible
defaults, or returns early. A panic is a bug, not an error handling
strategy.

### No unchecked indexing (libraries)
`indexing_slicing = "deny"` in library code. Use `.get()`, iterators,
or bounds-checked access. Application/GUI code may allow when indices
are structurally guaranteed.

---

## Linting

### Clippy configuration
```toml
[lints.rust]
unsafe_code = "forbid"
unused_must_use = "deny"

[lints.clippy]
all = { level = "deny", priority = -1 }
pedantic = { level = "warn", priority = -1 }

# Safety
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"

# Accepted pedantic overrides
too_many_arguments = "allow"
module_name_repetitions = "allow"
must_use_candidate = "allow"
missing_errors_doc = "allow"       # internal crates
missing_panics_doc = "allow"
cast_possible_truncation = "allow"
cast_sign_loss = "allow"
cast_precision_loss = "allow"
cast_possible_wrap = "allow"
```

Libraries (recentip) may escalate `pedantic` and `nursery` to `"deny"`.
Applications may keep them at `"warn"`.

### Formatting
Use `cargo fmt` defaults. No `rustfmt.toml`. Formatting is not a
style discussion — it's automated and enforced in CI.

---

## Error handling

### By project type
- **Libraries:** Custom `Error` enum with `thiserror`, `Result<T>`
  type alias, documented recovery per variant. Export the error type.
- **Applications/servers:** `anyhow::Result` with `.context()` for
  propagation. Domain error enums with `thiserror` at API boundaries
  (e.g. 400 vs 500 in HTTP handlers).

### Rules
- Propagate with context: `.with_context(|| format!("reading {}", path.display()))`
- Distinguish domain refusals from infrastructure errors at the type level
- Log errors at the handler/boundary where recovery decisions are made,
  not deep in library code
- `bail!()` for early returns with descriptive messages
- Never suppress errors silently — log at minimum

---

## Type safety

### Domain types over primitives
Parse at the boundary, typed internally. If a string represents a
fixed set of values, it's an enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum FindingStatus { New, Queued, Fixing, PrOpen, ... }
```

A typo in a status string is a runtime bug. A typo in
`FindingStatus::PrOpne` is a compile error.

### Newtypes for domain IDs
When a bare `i64` could mean repo ID, finding ID, or job ID
interchangeably — consider newtypes. At minimum, use named parameters
(never two `i64` positional params with different semantics).

### Named-field structs for mutations
Never pass multiple `Option<&str>` parameters with different semantics
(the `pr_url`-as-`verdict_reason` bug class). Use structs with named
fields or specific purpose-named functions.

### No `serde_json::Value` in internal data flow
External API responses are parsed into typed structs at the boundary.
Internal code operates on domain types. `serde_json::Value` is only
for genuinely unknown schemas (third-party APIs with no stable contract)
and JSON-text embedding in prompts.

### TypeScript strictness
When a project has a TypeScript frontend:
```json
{
  "strict": true,
  "noUncheckedIndexedAccess": true,
  "noUnusedLocals": true,
  "noUnusedParameters": true,
  "noFallthroughCasesInSwitch": true
}
```
ESLint: `@typescript-eslint/no-explicit-any: "error"`.

---

## Database / SQL

### Compile-time checked queries only
All SQL uses `sqlx::query!` / `sqlx::query_as!` macros, validated
against the schema at compile time. No `QueryBuilder`, no runtime
string construction, no `sqlx::query(` without `!`.

One documented exception: reading an external database whose schema
isn't available at compile time (e.g. omp's `agent.db`).

### Encapsulated data access
The database pool is private to the store module. All SQL goes
through typed store methods. Code outside the store cannot construct
or execute queries. Enforced by:
1. `fn pool()` is private (not `pub`)
2. CI lint: `grep` guards verify no pool leakage
3. Zero `QueryBuilder` in the codebase

### Specific methods over generic updates
No `update_job(id, &BigOptionalStruct)` with 15 Optional fields.
Instead: `complete_job(id, state, ...)`, `orphan_job(id, ...)` —
each a single compile-time checked query where every parameter is
required for that operation.

---

## Testing

### Philosophy
- **Deterministic:** no flaky tests. Simulate the outside world rather
  than mocking the code that reaches it. Fresh state per test (temp
  dirs, fresh DBs).
  - For this daemon that means *subprocess* simulation, not network
    simulation. Every external boundary it crosses is a CLI —
    `git` (34 call sites), `gh`, `glab`, `omp`, `npx` — and there is no
    outbound network client in the crate at all. `FakeBins` scripts
    those binaries so the real `Forge`/`dep_scan`/harness code runs and
    builds real argv against a simulated world.
  - Reach for `turmoil` only if that changes: it simulates a network
    between hosts (partitions, latency, loss), so it earns its place
    the day hunter talks to a forge REST API directly instead of
    shelling out, or grows a second process. Today the sole
    `tokio::net` call is the localhost listener, and the router tests
    drive `router()` directly, which is both faster and simpler.
  - For time, use `tokio::time` pause/advance rather than shortening
    intervals. Sleeping less is still sleeping, and a 120 s grace
    period cannot be shortened at all.
- **Behavioral:** test what the code does, not how it's structured.
  Assert on domain types, not string representations.
- **Coverage:** every changed line should be covered. Use `llvm-cov`
  for measurement (libraries).

### Patterns
- Integration tests in `tests/` directory
- `cargo nextest` for parallel execution and stress testing (libraries)
- Compliance traceability where applicable (`covers!()` macro)

### Shared test support (`hunter-rs/tests/support/mod.rs`)

Each integration test is its own crate, so a test file opts in with `mod
support;`. Reach for it before hand-rolling a fixture — every helper in
there replaced something that was being reinvented per file, usually
worse.

- `TempDir` — scratch directory removed on drop, *including on panic*.
  The `let _ = fs::remove_dir_all(..)` at the end of a test body leaks on
  every failure, which is exactly when you least want it to.
- `fresh_store` / `fresh_pool` — writable copy of the schema-only
  `dev.db`. Never open `dev.db` itself.
- `FakeBins` — scripted executables on `PATH` with an invocation log.
  This is the only seam for forge behaviour: the runners build their
  forge internally via `forge_for(repo.forge)`, so there is no trait to
  inject, and intercepting the subprocess exercises the real argv too.
  Match on the SUBCOMMAND (`ok_unless_action("gh", "pr comment", …)`) —
  a substring match on `"comment"` also matches `gh pr view --json
  ...,comments,...` and fails the wrong call.
  - `isolate()` when a test's point is that a binary is ABSENT; the
    default appends the real `PATH`, so a developer's own `npx` answers
    and the test passes vacuously.
  - `env(key, value)` for scoped environment variables, restored on
    drop. Hangs off `FakeBins` so one assertion covers both, and keeps
    every `unsafe` in the crate inside that one file.
- `ScriptedBackend` — a `Backend` whose `run()` is a closure over the
  worktree, so a test stages exactly what a worker would leave behind.
  `decide()` always grants, so tests need not satisfy the budget gate.
- `GitRepo::with_branch` — bare `origin` plus a clone with a published
  branch, the minimum for anything that fetches or adds a worktree.

The Rust suite is **nextest-only** (`cargo nextest run`, or `just
test`). Not for speed: `FakeBins` scripts binaries on `PATH` and scopes
environment variables, both process-global. Per-test process isolation
makes that private, so the suite carries no locking to coordinate it —
scaffolding you keep is scaffolding you maintain. `cargo test` would
race silently, so `require_process_isolation` asserts on
`NEXTEST_EXECUTION_MODE` and fails with an explanation instead.

`tests/runner_engage_test.rs` is the worked example: a real git repo, a
scripted `gh`, a scripted worker, the real `run_engage`, and assertions
on what was persisted. Start from it.

### Proof, not compilation

`cargo check` is not evidence that a change works. Neither is a green
suite that predates the change. A fix is verified when you have
**observed the new behaviour**: a test seen failing before and passing
after, or a command whose output demonstrates it.

- **Red first.** Every new or changed assertion must be watched failing
  against the pre-fix code. If you cannot make it fail, it is not
  testing your change. Invert the condition, confirm exactly the
  expected test breaks, revert.
- **Test-driven, in the strict sense.** Not "write a test first" but
  "write tests until the laziest implementation that passes them all is
  the correct one." Assume an adversarial implementer: if
  `render("{{A}} {{TYPO}}", …).is_err()` is the only test, `bail!()`
  passes it. The pressure to defeat that laziness is what forces you to
  enumerate the positive cases and the edge conditions — which is the
  step our regressions actually skipped, every time.
- Enumerate the **input domain**, not the requirement. A substitution
  function takes values that may contain the delimiter. A pipe emits
  bytes that may not be UTF-8. A worker either hangs or exits, with or
  without a ledger — that is a 2x2, so write four tests. All three of
  those were real regressions and all three are boring edge-case
  enumeration.
- **Do not test through a mock you invented.** A mocked 200 for an
  endpoint that returns 201 enshrines the bug instead of catching it;
  we shipped exactly that. Prefer the real collaborator or a
  simulation of it.
- The tests **stay around**, and that is most of the value. Every
  invariant broken in review here was one no test encoded: `render`
  had none, the whole Svelte UI had none. A suite is a ratchet against
  the next person changing an assumption you did not write down.

### Fixing a reported defect

Measured on this repo: of six agents fixing a review round, the two
that proved their fixes by *running* them introduced zero regressions;
the three that proved theirs by compiling introduced five between
them. Every one was a gap in what the author considered, not in what
they implemented. So:

- **Verify the report, then verify the fix.** Confirming the finding
  is real says nothing about whether the repair is right. Check your
  fix against inputs the report never mentioned.
- **Enumerate the failure boundary.** Any change that adds or moves an
  error, `bail!`, or terminal state must name the inputs that now land
  on the new side. "Reject unfilled placeholders" silently became
  "reject any PR body containing braces".
- **Cover the sibling branch.** Handling one arm of a state machine is
  an invitation to check the others. A worker that *hangs* unmetered
  and one that *exits* unmetered are the same accounting hole.
- **Read the contract before asserting about an interface.** Status
  codes, JSON shapes and endpoint behaviour are specified in
  `hunter-rs/API-CONTRACT*.md`. Cite the line; do not encode a belief.
- **Changing an invariant means finding its dependents.** Run
  `lsp references` (or grep) on the state you changed. Callers in other
  files still assume what you just stopped guaranteeing.

### Adversarial pass before shipping

Green gates are necessary and not sufficient: an entire review round's
regressions passed 187 tests, clippy, `svelte-check` and ESLint, and
were caught by a human-grade reader looking at the diff. Before
pushing a batch of fixes, run one reviewer over the diffs whose only
question is *"what input class did the author not consider?"* — not
*"is this correct?"*. That is the process that actually catches this
class, so run it deliberately rather than waiting for it to arrive
from outside.

### Mutation testing

Coverage says a line ran; a mutant says whether any test would notice
it changing. Every Rust project with a test suite gates its pull
requests on `cargo-mutants`, set up the way that has held up here
(`.github/workflows/mutants.yml`, `hunter-rs/.cargo/mutants.toml`).
Frontends are not required to.

- **The diff, not the tree.** `cargo mutants --in-diff` against the
  PR's base. A whole-tree run is hours (~1900 mutants in hunter-rs): per
  push it never finishes, gets demoted to advisory, and is ignored.
  Whole-tree is a manual audit, one module at a time (`--file`).
- **A gate, not a report.** A surviving mutant fails the PR. Answer it
  with the test that tells the code from its mutant, or by deleting the
  code nothing depends on; an equivalent mutant usually marks redundant
  code.
- **Testing nothing must fail, not pass.** Each of these reads exactly
  like a clean run:
  - diff paths not relative to the package (`git diff --relative` when
    the crate is in a subdirectory) filter out every mutant;
  - a failed `--list` looks like an empty one, so check its exit status;
  - a red baseline mutates nothing. Classify the exit code: 0 passes;
    2 (survivors), 3 (timeouts), 4 (baseline failed) and anything else
    fail.
- **Every mutant of the diff runs.** Split a large diff into shards
  (`--shard k/n`) small enough to finish under the step's time cap,
  behind one stable required check. Hitting the cap fails: part of the
  diff went untested. Set `--minimum-test-timeout`, or a caught mutant
  under CI load is reported as a timeout.
- **Its own workflow and concurrency group.** In CI's group every push
  cancels it, and it never finishes.
- **Configured in `.cargo/mutants.toml`**, so a local `cargo mutants`
  runs what CI runs: the test tool (with any nextest `-E` filter beside
  it — `cargo test` rejects one, failing the baseline), and the tests
  excluded for reading files outside the package, which cargo-mutants
  does not copy.
- **Exclusions are narrow and argued.** One mutant or one function per
  `exclude_re` entry, each with the reason the mutant is untestable or
  equivalent, checked against `cargo mutants --list` (a pattern that
  matches no mutant is silently inert). A whole file only when the
  reason holds for every mutant in it (generated code, a binary's argv
  parsing). Never exclude to turn a gate green.
- **Pinned tool version**, as for every CI tool.

---

## Dependencies

### Policy
- Minimal runtime deps. Every dependency is a maintenance liability
  and an attack surface.
- Feature-gate optional deps (`optional = true`)
- Pin CI tool versions to exact URLs/hashes, not mutable installers
- Common choices: `tokio`, `serde`, `anyhow`/`thiserror`, `tracing`,
  `axum` (server), `sqlx` (database), `clap` (CLI)

---

## Performance

- Profile before optimizing. Use `profiling::scope!()` or equivalent
  for hot paths.
- Pre-size collections: `Vec::with_capacity(n)` when size is known
- No needless allocations or copies
- LTO + `codegen-units = 1` for release builds (libraries and
  performance-critical applications)
- GPU shaders for per-frame per-entity math (graphics projects)
- Batch cross-boundary calls (JS↔WASM, network, IPC)

---

## CI / Enforcement

Every project enforces at minimum:
1. `cargo fmt --check`
2. `cargo clippy` with deny-level lints
3. `cargo nextest run --profile ci` — the command CI runs. Plain
   `cargo test` is not an equivalent here: `require_process_isolation`
   fails every test under it (see *nextest-only*, above).
4. The `cargo-mutants` gate on the PR's diff (see *Mutation testing*,
   above).

Additional per project type:
- **Frontend:** `npm run check`, `eslint` (strict), `vitest`
  - Run `svelte-check` through `npm run check`, never bare. Bare
    `svelte-check` picks a default config that covered 104 files where
    `--tsconfig tsconfig.app.json` covers 127, so it reports a clean tree
    for errors CI then fails on.
- **Libraries:** `cargo nextest`, `llvm-cov`, doc tests
- **Servers:** SQL encapsulation grep guards, parity tests
- **Git:** fast-forward only merges, no force-push to default branch

---

## Documentation

- Module-level doc comments explaining conventions and non-obvious
  decisions
- API contracts as separate markdown docs for cross-module interfaces
- Architecture decision records (ADRs) for significant choices,
  with rationale and rejected alternatives
- Code comments for *why*, not *what*
