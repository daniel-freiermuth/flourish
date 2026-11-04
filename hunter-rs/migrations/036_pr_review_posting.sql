-- Posting PR review rounds to the forge (future.md §8.8, slice 2).
--
-- A kept round with issues is posted as one COMMENT review under the
-- operator's forge login: each issue the PR's diff can place goes inline,
-- the rest into the review's body. Nothing is posted without a decision:
-- either the review was requested with `auto_post`, or a user approves the
-- round. The admin lane's PR review duty posts every approved round.

-- 1: a round recorded while this is on starts 'approved' (posted on the
-- PR review duty's next run); 0: it waits for a user. Toggling never touches rounds
-- already recorded.
ALTER TABLE pr_reviews ADD COLUMN auto_post INTEGER NOT NULL DEFAULT 0;

-- When a post began that may have left drafts of hunter's on the PR
-- (epoch ms). GitLab takes a review as draft notes of the operator's
-- login, published one by one; one a failed or stopped post did not
-- publish stays a draft, and the operator's next review by hand would
-- publish it under their name. Set before a post creates any, cleared
-- once that post published them all or the PR review duty deleted every draft of
-- hunter's there (each carries the own-comment mark). While set, the sync
-- deletes them before the review posts anything again.
ALTER TABLE pr_reviews ADD COLUMN drafts_left_at INTEGER;

-- NULL for a failed round and for one without issues: there is nothing to
-- post, ever. Otherwise 'pending' (a user decides), 'approved' (the PR
-- review duty's next run posts it), 'posted', or 'discarded' (a user decided never to, or
-- the round is of an earlier binding of the review, 035's `binding`, and
-- about another PR). Only rounds of the review's binding are posted.
ALTER TABLE pr_review_rounds ADD COLUMN post_state TEXT
    CHECK (post_state IN ('pending', 'approved', 'posted', 'discarded'));
-- Rounds recorded before posting existed wait for a decision like any new
-- one: posting is never decided for a user.
UPDATE pr_review_rounds SET post_state = 'pending'
WHERE verdict != 'failed'
  AND EXISTS (SELECT 1 FROM pr_review_issues i WHERE i.round_id = pr_review_rounds.id);

-- The newest failed post's reason, kept while the round retries and after
-- it fell back to 'pending'; cleared when a user approves it again.
ALTER TABLE pr_review_rounds ADD COLUMN post_error TEXT;
-- Failed posts since the round was last approved. At 3 the round falls
-- back to 'pending' for a user to decide; a post that failed while the
-- forge was unreachable counts nothing.
ALTER TABLE pr_review_rounds ADD COLUMN post_attempts INTEGER NOT NULL DEFAULT 0;
-- When the post landed (epoch ms) and the forge's id for it: GitHub's
-- review id, GitLab's draft note id of the body. NULL until posted.
ALTER TABLE pr_review_rounds ADD COLUMN posted_at INTEGER;
ALTER TABLE pr_review_rounds ADD COLUMN forge_review_ref TEXT;
-- When the first part of the round's post reached the PR (epoch ms): its
-- review, or an inline comment of a GitLab post that failed partway. A
-- round that landed may still fall back to 'pending' after failed posts,
-- but is never discarded: what is on the PR stays said.
ALTER TABLE pr_review_rounds ADD COLUMN landed_at INTEGER;

-- Which version of the file `line` numbers: 'RIGHT' the PR's head (the
-- default), 'LEFT' the base, for an issue about code the PR removes. NULL
-- for issues recorded before this column existed, read as 'RIGHT'.
ALTER TABLE pr_review_issues ADD COLUMN side TEXT CHECK (side IN ('RIGHT', 'LEFT'));
-- 1 once the issue went out as an inline comment on the PR's diff; 0 while
-- it went into the review's body or was never posted.
ALTER TABLE pr_review_issues ADD COLUMN inline INTEGER NOT NULL DEFAULT 0;
-- The forge's id of that inline comment (GitHub's comment node id,
-- GitLab's note id), read back from the PR after the post. NULL until a
-- read found it: a round is posted only once each of its inline comments
-- has its id, or is known gone.
ALTER TABLE pr_review_issues ADD COLUMN thread_ref TEXT;
-- When a read of the PR that lists every comment found the issue's inline
-- comment missing (epoch ms): deleted on the forge. Hunter never looks for
-- it again.
ALTER TABLE pr_review_issues ADD COLUMN thread_gone_at INTEGER;

-- The merge-base of the PR's head with its base branch, as the round
-- resolved it: the old side of the PR's diff. NULL for a round that failed
-- before resolving it. A round whose base is another commit (the head an
-- earlier round reviewed) numbers its LEFT lines by that commit, which the
-- diff does not show, so they go into the review's body.
ALTER TABLE pr_review_rounds ADD COLUMN merge_base_sha TEXT;
-- When a post last sent the round's review (epoch ms). A send can fail
-- after the forge acted on it (a timeout, a 5xx after the write), so while
-- no post is recorded (`forge_review_ref`), the next one looks for the
-- review on the PR, by the mark it carries, before sending it again.
ALTER TABLE pr_review_rounds ADD COLUMN review_sent_at INTEGER;
