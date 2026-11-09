Fix exactly ONE verified bug in the worktree at {{WORKTREE}} (repo
{{REPO_NAME}}, branch {{BRANCH}} — already checked out for you). Work only
inside this worktree. NEVER push. NEVER run project-wide formatters.

# Repository Context
{{REPO_NOTES}}

# The finding
```json
{{FINDING_JSON}}
```

# Protocol (evidence FIRST, commit per step)
1. On a fresh attempt, verify the bug still exists at HEAD (code moves). If
   it is already fixed upstream or the finding is demonstrably wrong, STOP:
   write NOT-A-BUG.md classifying why and explaining the evidence (see
   "Declining" below), commit nothing, and end.
   On a resumed attempt, inspect the existing commits and the previous
   blocker first. Preserve the committed implementation and proof; a fix
   already present in this branch is NOT evidence that the original finding
   was false. Continue from the retained checkpoint, resolving the reported
   blocker rather than rewriting or discarding completed work.
2. Prove it. Climb the evidence ladder as high as the code allows:
   - Rung 1: failing automated test in the repo's existing harness,
     written before the fix, failure observed and quoted.
   - Rung 2: scripted reproduction (script/curl/REPL trace), recorded
     verbatim in the PR description.
   - Rung 3: written data-flow trace with file:line references enumerating
     every relevant path.
   NEVER introduce a test framework as a side effect. No harness for this
   layer -> rung 2/3.
3. Fix it, minimally. No refactors, no drive-by cleanup. Match the repo's
   code style and commit-message convention.
4. Test tier follows the CODE'S SHAPE:
   - pure logic -> extract minimal pure function if needed + unit test;
   - cross-boundary contract -> test both sides;
   - UI-glue -> rung 2/3, note which E2E-level check would cover it.
5. Verify: run the affected package's tests/checks, then the full suite of
   the affected toolchain. All green, or the fix does not ship.
6. COMMIT AFTER EVERY STEP with descriptive messages. You may be killed at
   any moment — committed work survives, uncommitted work dies. The failing
   proof test may be committed alone as a checkpoint; once the fix makes it
   pass, amend the fix into that commit with a message that describes the
   fix (`git commit --amend -m "..."`), so no commit on the branch has a
   failing test. The subject of your last commit becomes the PR title: it
   must name the fix, not the test.

# Deliverables
- Commits on {{BRANCH}}, each one building and passing the tests on its own:
  the proof test lands in the same commit as the fix it proves.
- PR-DESCRIPTION.md at the worktree root, NOT COMMITTED (it becomes the PR
  body): what breaks; why (code evidence); the INTENDED behavior and how you
  know it is intended; evidence rung achieved and the proof itself (or where
  it lives); verification performed with observed results; what you
  deliberately did NOT change.
- Missing proof is NOT a false finding. Write NOT-A-BUG.md only when evidence
  establishes that the finding is invalid; if you cannot reach at least rung
  3 with an airtight argument or cannot verify the affected change, write
  BLOCKED.md with the precise missing proof/prerequisite/decision and the
  evidence supporting that conclusion, instead of shipping a half fix.

# Declining
{{DECLINE_CLASSIFICATION}}
