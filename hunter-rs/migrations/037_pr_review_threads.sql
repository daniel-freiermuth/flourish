-- Replies in, resolutions out (future.md §8.8, slice 3).
--
-- A later round of a watched review reads the threads its inline comments
-- opened on the PR, judges every earlier issue against the new commits and
-- the replies, and, once its post goes out, makes each judged issue's
-- thread say what the newest judgement is: answered and resolved when the
-- issue is addressed or rebutted, answered and reopened when hunter had
-- settled it and it is open again. Only threads hunter opened are ever
-- answered or resolved; the issue's `thread_ref` is how it knows which
-- those are. A thread is always found by it, on a read of every thread of
-- the PR: the id replying and resolving take is never kept, so one a human
-- deleted is found gone (`thread_gone_at`) instead of failing every post
-- that would answer it.
--
-- Nothing here records an issue's status: it is derived, by the view at
-- the end, from the verdicts. What is recorded is what each round said
-- (`pr_review_verdicts`) and what hunter has said in each thread (the
-- issue's `answer` and `resolved_at`); a post converges the second to the
-- first.

-- Hunter's standing answer in the thread: the verdict its newest reply
-- there gave. NULL while hunter's comment alone speaks, which says the
-- issue stands, and once a human reopened a thread hunter had resolved.
ALTER TABLE pr_review_issues ADD COLUMN answer TEXT
    CHECK (answer IN ('addressed', 'rebuttal-accepted', 'open'));
-- When hunter resolved the thread (epoch ms); NULL while it is not, and
-- again once hunter or a human unresolved it.
ALTER TABLE pr_review_issues ADD COLUMN resolved_at INTEGER;
-- When a read found the thread hunter resolved unresolved again (epoch
-- ms; every reopen later than the one before): a human disagreed, which
-- voids every verdict given over the thread as it stood before. Kept
-- after a later round settles the issue again.
ALTER TABLE pr_review_issues ADD COLUMN reopened_at INTEGER;
-- The thread's replies after hunter's comment as the newest read saw them,
-- screened to the authors a review trusts (who can push, the configured
-- review bots, the PR's author): a JSON array of {"author", "body", "own"},
-- `own` true for hunter's own replies (shown under the author "hunter",
-- but told apart by the flag alone: a person's login may be "hunter" too).
-- Replaced on every read.
ALTER TABLE pr_review_issues ADD COLUMN replies_json TEXT NOT NULL DEFAULT '[]';
-- How many replies that read dropped: they are counted, never shown, and
-- never reach a prompt.
ALTER TABLE pr_review_issues ADD COLUMN hidden_replies INTEGER NOT NULL DEFAULT 0;

-- What a kept round said about each earlier issue of its chain. An
-- issue the round did not mention has no row: silence is no verdict, so it
-- stays as it was. A failed round records none.
CREATE TABLE pr_review_verdicts (
    round_id INTEGER NOT NULL REFERENCES pr_review_rounds(id) ON DELETE CASCADE,
    issue_id INTEGER NOT NULL REFERENCES pr_review_issues(id) ON DELETE CASCADE,
    verdict TEXT NOT NULL CHECK (verdict IN ('addressed', 'rebuttal-accepted', 'open')),
    -- The reviewer's one sentence of evidence.
    note TEXT NOT NULL DEFAULT '',
    -- The issue's `reopened_at` as the round read it (NULL: not reopened
    -- then). A verdict only counts over what its round saw: once a later
    -- reopen is recorded, the round judged a thread it never saw reopened
    -- -- one the PR review duty found while the reviewer ran -- and its verdict
    -- counts as none.
    seen_reopened_at INTEGER,
    PRIMARY KEY (round_id, issue_id)
);
CREATE INDEX pr_review_verdicts_issue ON pr_review_verdicts(issue_id);

-- Each issue's judgements, derived from the verdicts and nowhere stored.
--
-- `status` is hunter's: the newest verdict of a round not discarded --
-- 'addressed', 'rebutted' or 'open' -- as long as no reopen was recorded
-- since that round read the issue; 'reopened' while a human's reopen
-- stands that no verdict was given over yet (a verdict over the thread as
-- it stood before counting as none). Any verdict over the reopen settles
-- it, 'open' too: the reopen asked for a judgement, and got one. A
-- pending round's verdicts count: the next round is told them, and
-- discarding the round takes them back.
--
-- `target_round_id` is the PR's: the round whose verdict the issue's thread
-- is to show, the newest released one -- approved, posted, with nothing to
-- post (NULL post_state), or landed in part -- unless a human reopened the
-- thread after its round read it. NULL when no released verdict stands.
CREATE VIEW pr_review_issue_state AS
SELECT l.issue_id,
       CASE WHEN l.reopened_at IS jv.seen_reopened_at
            THEN CASE jv.verdict WHEN 'addressed' THEN 'addressed'
                                 WHEN 'rebuttal-accepted' THEN 'rebutted'
                                 ELSE 'open' END
            WHEN l.reopened_at IS NOT NULL THEN 'reopened'
            ELSE 'open' END AS status,
       l.judged_round_id,
       CASE WHEN l.reopened_at IS tv.seen_reopened_at
            THEN l.released_round_id END AS target_round_id
FROM (
    SELECT i.id AS issue_id, i.reopened_at,
           (SELECT v.round_id FROM pr_review_verdicts v
            JOIN pr_review_rounds r ON r.id = v.round_id
            WHERE v.issue_id = i.id AND r.post_state IS NOT 'discarded'
            ORDER BY r.finished_at DESC, r.id DESC LIMIT 1) AS judged_round_id,
           (SELECT v.round_id FROM pr_review_verdicts v
            JOIN pr_review_rounds r ON r.id = v.round_id
            WHERE v.issue_id = i.id AND r.post_state IS NOT 'discarded'
              AND (r.post_state IS NOT 'pending' OR r.landed_at IS NOT NULL)
            ORDER BY r.finished_at DESC, r.id DESC LIMIT 1) AS released_round_id
    FROM pr_review_issues i
) l
LEFT JOIN pr_review_verdicts jv ON jv.round_id = l.judged_round_id AND jv.issue_id = l.issue_id
LEFT JOIN pr_review_verdicts tv ON tv.round_id = l.released_round_id AND tv.issue_id = l.issue_id;
