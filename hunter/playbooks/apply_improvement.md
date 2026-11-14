Apply exactly ONE suggested improvement in the worktree at {{WORKTREE}} (repo
{{REPO_NAME}}, branch {{BRANCH}} — already checked out for you). Work only
inside this worktree. NEVER push. NEVER run project-wide formatters.

This playbook handles IMPROVEMENTS (dep_update, test_gap, refactor), NOT bugs.
For bugs, see fix.md.

# Repository Context
{{REPO_NOTES}}

# The improvement
```json
{{FINDING_JSON}}
```

# Protocol (assess FIRST, commit per step)

## 1. Assess value and feasibility

Read the finding and determine if the improvement is:
- **Valuable**: Does it add real value (security, performance, coverage, maintainability)?
- **Safe**: Low risk of breaking existing functionality?
- **Actionable**: Can you apply it now without major blockers?

### Verify every factual claim before declining or blocking

DECLINED.md and BLOCKED.md exist to record a VERIFIED conclusion, not a
plausible-sounding guess. Before writing either, verify every external factual
claim you rely on with an actual command or fetched source — NEVER from
memory or training data, which goes stale and hallucinates specifics:

- **Version gaps / upgrade paths**: list the ACTUAL published version history
  (`npm view <pkg> versions --json`, `cargo info <crate>` or the crates.io
  index, `pip index versions <pkg>`, or the registry API directly). A claim
  like "no version exists between X and Y" or "the only path is a big jump"
  MUST be backed by this list, quoted in your reasoning.
- **Removed/changed APIs**: read the ACTUAL source of the dependency at the
  target version (GitHub, unpkg, docs.rs) — do not assume an API was removed
  or changed without reading the current code at that version.
- **Peer dependency / compatibility constraints**: read the ACTUAL manifest
  (`package.json`/`Cargo.toml`/`pyproject.toml`) of the blocking package at
  its latest version — not a docs page, which may be stale or aspirational.

If you cannot verify a claim this way (network unavailable, private
registry), say so explicitly in BLOCKED.md rather than asserting it as fact.

If NOT valuable or safe (verified), STOP: write DECLINED.md explaining why,
citing the verification performed (e.g., "dependency pinned for
compatibility — confirmed via `<command>`: `<relevant output>`").

If blocked by external factors you can't resolve (conflicting patches, a
VERIFIED incompatibility, requires architectural decisions), STOP: write
BLOCKED.md with exactly what's missing and the verification evidence for the
blocking claim.

## 2. Apply the improvement

### For `type = 'dep_update'`:

a. **Check for local patches** in patches/, .patch files, or vendored code
b. **Upgrade the dependency** -- every one `detail` lists, when the finding
   is a group (packages released together, or one tool pinned in several
   files): they move in this one change, each to its own target, in every
   file named. Upgrade to the target version -- or to the highest
   version you can safely reach this pass if the full target isn't
   achievable (a peer-dependency/toolchain constraint, needs a coordinated
   bump elsewhere). If you settle for anything below the finding's own
   `latest_version`, treat that gap as real, mandatory work for step 5
   below -- see there for why this is easy to miss.
c. **Handle patches**:
   - If patches exist, try to rebase/update them for the new version
   - If patches conflict or fail, document in BLOCKED.md
   - If patches are no longer needed (fix upstreamed), remove them
d. **Install and verify** the new version resolves correctly
e. **Commit**: "deps: upgrade {package} from {old} to {new}"

### For `type = 'test_gap'`:

a. **Verify the gap** - confirm the contract/edge case is untested
b. **Add tests** covering the missing scenario
   - Use existing test harness and conventions
   - NEVER introduce new test frameworks as a side effect
c. **Run tests** - verify new tests pass and existing tests still pass
d. **Commit**: "test: add coverage for {contract/edge-case}"

### For `type = 'refactor'`:

a. **Verify the smell** - confirm the code issue exists
b. **Apply refactoring** (extract method, remove duplication, simplify)
   - Keep changes minimal and mechanical
   - Match existing code style
c. **Run tests** - verify no behavior change
d. **Commit**: "refactor: {what} in {where}"

## 3. Verify

Run the affected package's tests/checks. For dependency updates, also:
- Check that lockfiles updated correctly
- Verify no new deprecation warnings
- Spot-check one usage of the upgraded dependency

All green, or the improvement does not ship.

## 4. Commit discipline

COMMIT AFTER EVERY STEP with descriptive messages. You may be killed at any
moment — committed work survives, uncommitted work dies.

## 5. Be explicit about anything you deferred

Two distinct triggers below. Both are real deferred work; neither is optional
when it applies.

- **You didn't reach the finding's own target.** The single most common case,
  and the easiest to miss precisely because it feels like "I already did the
  work": for `type = 'dep_update'`, if you shipped anything below the
  finding's `latest_version` (stopped short at an intermediate version due to
  a peer-dependency/toolchain constraint), the gap between what you shipped
  and that original target is NOT closed -- it is deferred work, every time,
  unless the constraint is structural and permanent (the package genuinely
  stopped publishing further versions, or the target version doesn't exist).
  "Requires a coordinated bump I didn't attempt this pass" is NOT permanent.
- **Something else is now clearly worth doing.** E.g. a dep upgrade lands but
  a bigger DSL/API migration it enables is deliberately left for later, a
  flag is left off pending a follow-up, a patch is dropped but its underlying
  issue still needs a real fix upstream.

Either way: name it precisely, with numbers/versions/file references, in
PR-DESCRIPTION.md's "what you deliberately did NOT change" section below --
not "some further work may be possible" hedging. A dedicated pass reviews
every PR once it MERGES (its final shape, after any review discussion, is
the only reliable signal of what's actually still open) and turns anything
real you named here into a fresh, queryable finding automatically. Vague
prose here produces nothing; specific prose does the job without you having
to hand-author a JSON entry that a PR-open-time snapshot would get wrong
anyway if review pushes the PR further before it merges.

# Deliverables

- **Commits on {{BRANCH}}** (upgrade/test/refactor commits)
- **PR-DESCRIPTION.md** at the worktree root, NOT COMMITTED. Its first line is
  `Title: <the PR title>`, naming the change; then a blank line; the rest
  becomes the PR body:
  - What improved (dep upgrade, test coverage, refactoring)
  - Why valuable (security, correctness, maintainability)
  - What changed (version bump + patch rebase, new test cases, simplified code)
  - Verification performed with observed results
  - What you deliberately did NOT change

OR if not actionable:

- **DECLINED.md**: Why the improvement isn't worth doing (already covered, too
  risky, not valuable enough, dependency pinned for compatibility), with the
  verification command + output backing any factual claim, classified as
  below
- **BLOCKED.md**: What's blocking you (patch conflicts, missing arch decisions,
  a verified incompatibility needing human review) with exactly what is
  needed to unblock, and the verification command + output for the claim

## Classifying DECLINED.md
{{DECLINE_CLASSIFICATION}}

# Improvement-specific guidance

## Dependency updates with patches

If local patches exist:
1. Note patch purpose (bug fix, API adaptation, feature backport)
2. Check if target version makes patch obsolete: fetch the ACTUAL upstream
   source file(s) the patch touches at the target version and diff against
   what the patch expects — don't infer this from changelogs or release notes
3. If patch still needed, rebase to new version:
   - Apply patch to new version
   - Resolve conflicts
   - Test that patch still serves its purpose
4. If patch conflicts and you can't resolve: BLOCKED.md "patch rebase failed,
   needs manual resolution" + show conflict details

## Test gaps

Focus on:
- Edge cases and boundaries (empty, null, first/last, max)
- Error paths (invalid input, failure states)
- Contracts between components (API shape, invariants)

NOT:
- Tests for implementation details
- Redundant coverage of already-tested paths
- Tests that would require major scaffolding

## Refactoring

Safe refactorings only:
- Extract duplicate code
- Simplify complex conditionals
- Remove dead code (verify unused with grep)
- Rename for clarity (use LSP rename if available)

NOT:
- Architectural changes
- Pattern shifts
- Abstraction introduction without clear duplication

# Success criteria

The improvement is:
- **Applied correctly** (upgraded, tested, refactored)
- **Verified working** (tests pass, no regressions)
- **Well-documented** (clear PR description)
- **Minimal** (only what the finding suggests, no scope creep)

OR explicitly declined/blocked with clear reasoning.
