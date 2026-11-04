-- Posting PR review rounds to the forge (future.md §8.8, slice 2).
--
-- A successful round with issues is posted as one COMMENT review under the
-- operator's forge login: each issue the PR's diff can place goes inline,
-- the rest into the review's body. Nothing is posted without a decision:
-- either the review was requested with `auto_post`, or a user approves the
-- round. The admin lane's PR sync posts every approved round.

-- 1: a round recorded while this is on starts 'approved' (posted on the
-- next PR sync); 0: it waits for a user. Toggling never touches rounds
-- already recorded.
ALTER TABLE pr_reviews ADD COLUMN auto_post INTEGER NOT NULL DEFAULT 0;

-- NULL for a failed round and for one without issues: there is nothing to
-- post, ever. Otherwise 'pending' (a user decides), 'approved' (the next
-- PR sync posts it), 'posted', or 'discarded' (a user decided never to).
ALTER TABLE pr_review_rounds ADD COLUMN post_state TEXT
    CHECK (post_state IN ('pending', 'approved', 'posted', 'discarded'));
-- Rounds recorded before posting existed wait for a decision like any new
-- one: posting is never decided for a user.
UPDATE pr_review_rounds SET post_state = 'pending'
WHERE verdict = 'ok'
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

-- Which version of the file `line` numbers: 'RIGHT' the PR's head (the
-- default), 'LEFT' the base, for an issue about code the PR removes. NULL
-- for issues recorded before this column existed, read as 'RIGHT'.
ALTER TABLE pr_review_issues ADD COLUMN side TEXT CHECK (side IN ('RIGHT', 'LEFT'));
-- The forge's id of the inline comment this issue was posted as (GitHub's
-- comment node id, GitLab's draft note id); NULL when it went into the
-- review's body or was never posted.
ALTER TABLE pr_review_issues ADD COLUMN thread_ref TEXT;
