Your change ({{BASE}}..HEAD on branch {{BRANCH}} in {{WORKTREE}}) passed its
pre-PR review and is about to ship. It was committed step by step, so it
could survive being killed, and every review answer added more commits. Now
rewrite that history into a few logical commits a reviewer can read.

# Rules
- Rewrite only {{BASE}}..HEAD, on the current branch {{BRANCH}}. NEVER push.
  Do not touch {{BASE}} or anything before it.
- The final tree must stay byte-identical to the reviewed commit
  {{REVIEWED}}: `git diff --quiet {{REVIEWED}} HEAD` must succeed when you
  are done, and nothing tracked may be left uncommitted. Change history,
  never content. If it does not end identical, your history is thrown away
  and the original commits ship.
- Leave PR-DESCRIPTION.md untouched and untracked. Add no other files.
- Do not run test suites: the commits need not each pass the tests, they
  must tell a clean story.

# The story to tell
- A few logical commits, each one change a reviewer can follow.
- Where the change has a proof test, put it in its own commit before the
  fix it proves.
- Fold every review answer into the commit it corrects; no "address
  review" commits.
- Descriptive messages in the convention of this repository's history
  (`git log {{BASE}} -20`): what changed and why.

One way that keeps the tree: `git reset --soft {{BASE}}`, then stage and
commit in steps (`git add -p` or by path), finishing with
`git diff --quiet {{REVIEWED}} HEAD` to check. If anything goes wrong, run
`git reset --hard {{REVIEWED}}` and stop: the original commits ship.

Then stop.
