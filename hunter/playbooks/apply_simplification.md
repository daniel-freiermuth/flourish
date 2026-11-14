You are following up on a SIMPLIFICATION proposal in the worktree at {{WORKTREE}} (repo
{{REPO_NAME}}, branch {{BRANCH}} — already checked out for you). Work only inside this
worktree. NEVER push. NEVER run project-wide formatters.

This playbook is deliberately NOT apply_improvement.md's contract. A human queued this
proposal knowing it changes a design: replacing a data structure, deleting a layer or
removing a variant across several files is expected here, not a reason to decline. What is
NOT expected is growth: the change must leave the code with fewer concepts than it found.

# Repository Context
{{REPO_NOTES}}

# The proposal
```json
{{FINDING_JSON}}
```

# Protocol

## 1. Re-verify before doing anything

Note the commit you start from (`git rev-parse HEAD`); step 3 measures against it.

The proposal rests on an enumeration ("one implementation", "never constructed", "every
caller passes X", "the feature was removed in <commit>"). The code may have moved since the
scan, and the scan may have missed a user. Redo the enumeration yourself, across the WHOLE
repository: source, tests, scripts, config, docs, persisted or serialized forms (database
rows, files, wire formats that may still carry the old shape), and anything reached by
string name, reflection or external API.

If a payer exists that the proposal did not count, and the simplification no longer holds
with it, STOP: write DECLINED.md naming that payer with file:line.

## 2. Decide the deliverable

### (a) The full simplification — the default when the enumeration is complete
The evidence is countable and you just confirmed it: make the change. Behavior the remaining
payers rely on must not change.
- Delete first, then repair what the deletion breaks. Do not build the replacement beside the
  old thing and leave both
- Tests that only exercised what you removed go with it, and nothing else does
- Tests of behavior that survives keep what they ASSERT. Adapting how such a test calls the
  code is expected and legitimate: a dropped parameter, a constructor with fewer fields, an
  `Option` that became a plain value, a helper's tests moving to the caller it was factored
  into. Report each under "tests changed". But if an assertion, an expected value or an
  input's meaning had to change to keep a surviving test green, behavior changed: stop, and
  treat it as (b)
- Docs and comments are updated to match, not trimmed: remove what describes the deleted
  thing, keep everything else
- No new abstraction, helper layer or dependency. If the simpler design needs one, it is
  (b)
- For `parallel-solutions`, (a) is only for a move nobody can observe: the survivor is used
  as it stands and behaves the same for the users you move. If the survivor has to be
  adapted, or the moved users would see anything different, it is (b): the plan names each
  difference and who sees it. If moving them would need options, modes or callbacks that only
  they use, keeping both is the answer, and that is a DECLINED.md
- Never loosen a type to get there: no enum turned into a string or int, no struct into a
  map or tuple, no typed data into untyped JSON. The compiler or type checker must enforce
  at least as much after the change as before. A proposal that only works by loosening a
  type is declined

### (b) A plan only — when a human has to decide first
For a `requirement-question`, or when the simpler design changes observable behavior, needs a
data migration, or has more than one reasonable shape: do NOT attempt the code change.
Commit ONLY a plan document under `docs/` (e.g. `docs/SIMPLIFICATION-<topic-slug>.md`):
- What exists and why it was built, the payers today, the simpler design, what would be
  deleted, what breaks and for whom, and the exact question the human must answer
- This MUST be a real commit (a shipped PR always needs at least one)
- Start the plan commit's subject with `[RFC]`. The PR title is taken from the commit
  subject, not from PR-DESCRIPTION.md, so this is what marks the PR as a decision to make
  rather than a change to merge

COMMIT PER STEP: you may be killed at any moment, and committed work survives while anything
only in your head does not.

### Leaving (a) after you started it
Everything committed on the branch is shipped unless DECLINED.md or BLOCKED.md exists. So if
you began (a) and then find it has to be (b) (an assertion had to change, the simpler design
needs a new abstraction, a decision turned up), go back first: `git reset --hard <start
commit>`, then write the plan. The branch must hold ONLY the plan. A plan committed on top of
a half-applied change would ship both, untested, under an `[RFC]` title.

## 3. Verify what you actually changed

**If you made the change (a):** run the affected package's build, lints and tests. All green,
or the simplification does not ship. If you cannot get them green, find out why before doing
anything else: a failure that shows a payer the proposal missed is a DECLINED.md (step 1); one
that shows behavior had to change is (b), after the reset above; anything else you cannot
resolve is a BLOCKED.md, with the failing command, its output and what is needed. Never leave
a red branch without one of the two files: it would be shipped as it is.

Then measure it: `git diff --stat <start commit>..HEAD`,
and from it the lines added and removed in PRODUCTION code alone. Tests, docs, comments,
fixtures and generated files do not count toward the result, in either direction. If the
production code did not shrink clearly, it is not a simplification: say so in DECLINED.md with
the numbers, rather than shipping a rewrite of equal size or one that only looks smaller
because tests or docs went.

**If you only wrote a plan (b):** there is nothing to run and nothing to measure, and the
shrink rule does not apply: a plan changes no production code, and that is not a reason to
decline. Ship the plan. Check instead that every claim it makes is the evidence you
re-verified in step 1, and that its estimate of what would be deleted counts production code
only.

# Deliverables

- **Commits on {{BRANCH}}** — at least one in either path
- **PR-DESCRIPTION.md** at the worktree root, NOT COMMITTED (becomes the PR body):
  - Which of (a) / (b) you chose and why
  - Why the complexity existed, and the re-verified evidence that its reason is gone
  - (a) What was removed: lines added/removed in production code, with tests and docs
    reported separately, and the concepts that no longer exist (types, variants, states,
    parameters, branches). (b) What WOULD be removed, as an estimate on the same terms
  - (a) Tests deleted or changed, each with the reason that is legitimate
  - Verification performed with observed results
  - What you deliberately did NOT change, and what a human still needs to decide

OR if re-verification showed the premise no longer holds, or the change you made (a) did not
shrink the production code. Never for a plan (b), which is shipped:

- **DECLINED.md**: what you re-verified and why the simplification is not worth doing, with
  the command/output backing the claim, classified as follows:

{{DECLINE_CLASSIFICATION}}

OR if the change (a) cannot be made green and neither of the above explains it:

- **BLOCKED.md** at the worktree root, NOT COMMITTED: the failing command and its output, what
  you tried, and exactly what is needed to continue. The work is held for a human; your
  commits stay as they are

# What this is not

Not a rewrite to taste. Every deletion is justified by a payer that is gone, shown by a
search you ran. Where the reason for a piece of code cannot be found and its users cannot be
fully enumerated, it stays.
