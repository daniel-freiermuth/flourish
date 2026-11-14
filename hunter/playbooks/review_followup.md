An independent reviewer read your change ({{BASE}}..HEAD in {{WORKTREE}})
before it ships and raised the issues below, blocking ones first. Nothing has been
pushed yet. The same rules as before apply: work only inside this worktree,
NEVER push, NEVER run project-wide formatters, commit after every step.

# Issues (the reviewer's words, verbatim)
Each issue has the `id` your answer names it by. One raised in an earlier
round may carry your earlier `answer` and, under `still_open`, why the
reviewer did not accept it.
```json
{{ISSUES}}
```

# What to do with EVERY issue
Answer every issue below, blocking or not: `blocking` only says how serious
the reviewer judged it. An issue left unanswered stays open and is raised
again next round.
Defects the reviewer judged outside this change's scope (follow-ups) are not
in this list and not yours to fix here: they go to the PR body for later.
1. Verify it against the code. The reviewer was told to distrust your
   change, so it can be wrong; it can also be right about something you did
   not consider. Decide from the code, not from either party's confidence.
2. A real issue: fix it with commits on this branch, under the same evidence
   rules as your original task (a failing proof test written first where the
   code allows, then the fix in the same commit as that test, then the
   affected checks green). Every commit must build and pass the tests on
   its own.
3. An issue you judge wrong: do NOT silently skip it. Rebut it with
   evidence: the file:line references, test output or trace that shows why
   it does not hold. A rebuttal without code evidence does not count, and
   the reviewer will keep the issue open.
4. If the review shows the change is wrong at its root, say so the way your
   original task allows: {{DECLINE_FILE}} when the finding itself turns out
   invalid, BLOCKED.md when it cannot be finished without something only a
   human can provide. Either one stops the PR.

# Answers file — INCREMENTAL, you may be killed at any moment
Create the answers file (named at the end) containing `[]` as your VERY
FIRST action. After EACH issue you settle, rewrite the complete file with
every answer so far, one entry per issue:

```json
[{"id": 12, "answer": "addressed", "text": "how it is fixed", "commit": "sha"}]
```

- `answer`: `addressed` (fixed by your commits) or `rebutted` (it does not
  hold).
- `text`: for `addressed`, what you changed and how it fixes the issue; for
  `rebutted`, the code evidence why the issue does not hold. A few
  sentences, not a log.
- `commit`: the commit that fixes it; leave it out for a rebuttal.

The next review round reads your answers next to the issues, checks every
fix and rebuttal against the code, and judges each issue by its id. The
answers file is outside the worktree: do NOT commit it, and do NOT add a
review section to PR-DESCRIPTION.md.

# Deliverables
- Commits for every issue you addressed.
- The answers file, one entry per issue above.
- PR-DESCRIPTION.md kept current (still NOT committed): it must describe the
  change as it now stands, not the review.

Then stop. The whole change is reviewed again before it ships.

Answers file: {{ANSWERS_FILE}}
