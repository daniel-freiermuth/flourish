An independent reviewer read your commits ({{BASE}}..HEAD in {{WORKTREE}})
before they are pushed to the PR and raised the issues below, blocking ones
first. Nothing has been pushed yet. The same rules as before apply: work
only inside this worktree, NEVER push, NEVER run project-wide formatters,
commit after every step.

# Issues (the reviewer's words, verbatim)
```json
{{ISSUES}}
```

# What to do with EVERY issue
Answer every issue below, blocking or not: `blocking` only says how serious
the reviewer judged it. An issue left unanswered is raised again next round.
Defects the reviewer judged outside this change's scope (follow-ups) are not
in this list and not yours to fix here: they go to your reply for later.
1. Verify it against the code. The reviewer was told to distrust your
   change, so it can be wrong; it can also be right about something you did
   not consider. Decide from the code, not from either party's confidence.
2. A real issue: fix it with commits on this branch, under the same rules
   as the feedback you answered (verify first, minimal diff, run the
   affected tests). Every commit must build and pass the tests on its own.
3. An issue you judge wrong: do NOT silently skip it. Rebut it with
   evidence: the file:line references, test output or trace that shows why
   it does not hold. A rebuttal without code evidence does not count, and
   the reviewer will raise the issue again.
4. If the review shows the PR is wrong at its root, say so the way your
   original task allows: {{DECLINE_FILE}} to withdraw the PR, BLOCKED.md
   when it needs a decision only a maintainer can make. Either one stops
   the push.

# Deliverables
- Commits for every issue you addressed.
- PR-REPLY.md kept current (still NOT committed): it is posted on the PR
  with the commits, so it must describe them as they now stand. Append, or
  update if it already exists, a "## Review" section listing every issue
  above by its summary, either as addressed (and how, with the commit) or as
  rebutted (with the evidence). Keep the entries of earlier rounds. The next
  review round reads that section, checks your fixes and rebuttals against
  the code, and does not raise a rebutted issue again unless it can refute
  the rebuttal with new code evidence.

Then stop. The whole change is reviewed again before it is pushed.
