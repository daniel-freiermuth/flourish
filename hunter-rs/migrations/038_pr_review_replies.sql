-- Watch also acts on replies and reopens (future.md §8.8).
--
-- A reply in one of hunter's threads is somebody arguing with the review,
-- and before this a watched review only looked at replies when the author
-- pushed again: a rebuttal on a PR nobody pushes to went unanswered, and so
-- did a thread a human reopened. Now the admin lane's PR review duty reads the
-- threads of every watched review, and news in an issue's thread that no
-- standing verdict was given over -- a trusted reply newer than any its
-- verdict saw, or a reopen -- makes the next round due even at an
-- unchanged head; that round judges the earlier issues against the replies
-- alone, and answers each one it still finds open.
--
-- A verdict counts over the thread it was given over, and only that: as
-- for reopens (037's `seen_reopened_at`), each verdict keeps the newest
-- reply its round was shown, and a reply newer than that leaves the issue
-- awaiting judgment again, whatever the verdict said, and keeps a post from
-- answering or resolving the thread with it. So a reply on an issue a
-- still unposted round settled is judged by the next round before
-- anything resolves the thread, and discarding a round takes back what its
-- verdicts saw along with the verdicts.
--
-- Which replies are news is one rule, for what makes a round due and for
-- what keeps a post from a thread alike: a reply under hunter's own
-- resolution, while it stands, asks for nothing (one that disagrees
-- reopens the thread); any other one is news, whoever resolved the thread
-- on the forge -- GitLab's "Comment & resolve thread" is a rebuttal and a
-- resolution in one action, and the rebuttal is answered.

-- Epoch ms of the newest trusted reply by somebody other than hunter in
-- the issue's thread, as the newest read (a round's or the PR review duty's) saw
-- it, resolved on the forge or not; NULL when there is none. Rewritten by
-- every read.
ALTER TABLE pr_review_issues ADD COLUMN newest_reply_at INTEGER;

-- The newest reply by somebody other than hunter in the issue's thread as
-- the round was shown it (epoch ms); NULL when it was shown none. The
-- verdict was given over the replies up to it.
ALTER TABLE pr_review_verdicts ADD COLUMN seen_reply_at INTEGER;
-- When the thread, as the round was shown it, ended in a reply that was
-- not hunter's: that reply's time (epoch ms). Somebody is waiting for an
-- answer, and the verdict is posted there as hunter's answer even when it
-- changes nothing ("Still open"). NULL when hunter spoke last.
ALTER TABLE pr_review_verdicts ADD COLUMN answers_reply_at INTEGER;
-- The newest reply hunter's answers in the thread have answered (epoch
-- ms): a verdict that answers one no newer is said already, whichever
-- round's post said it.
ALTER TABLE pr_review_issues ADD COLUMN answered_through INTEGER;

-- 037's view, with what each issue awaits.
--
-- `awaits_since` is the news in the issue's thread no verdict was given
-- over yet (epoch ms; the newer of the two when both): a reopen no verdict
-- was given over (`status` 'reopened'), or a reply newer than the newest
-- one the issue's newest verdict was shown -- any reply, on an issue no
-- round judged -- unless hunter's resolution of the thread stands
-- (`resolved_at`, which a read finding the thread unresolved clears). NULL
-- when there is none, and for an issue without a thread on the PR. A
-- watched review is due by it, and a round at the reviewed head judges
-- the issues it marks; `ThreadSeen::news_at` is the same rule over a
-- fresh read.
DROP VIEW pr_review_issue_state;
CREATE VIEW pr_review_issue_state AS
SELECT n.issue_id, n.status, n.judged_round_id, n.target_round_id,
       MAX(COALESCE(n.reopen_news, n.reply_news),
           COALESCE(n.reply_news, n.reopen_news)) AS awaits_since
FROM (
    SELECT l.issue_id,
           CASE WHEN l.reopened_at IS jv.seen_reopened_at
                THEN CASE jv.verdict WHEN 'addressed' THEN 'addressed'
                                     WHEN 'rebuttal-accepted' THEN 'rebutted'
                                     ELSE 'open' END
                WHEN l.reopened_at IS NOT NULL THEN 'reopened'
                ELSE 'open' END AS status,
           l.judged_round_id,
           CASE WHEN l.reopened_at IS tv.seen_reopened_at
                THEN l.released_round_id END AS target_round_id,
           CASE WHEN l.threaded AND l.reopened_at IS NOT NULL
                     AND l.reopened_at IS NOT jv.seen_reopened_at
                THEN l.reopened_at END AS reopen_news,
           CASE WHEN l.threaded AND l.resolved_at IS NULL
                     AND l.newest_reply_at > COALESCE(jv.seen_reply_at, 0)
                THEN l.newest_reply_at END AS reply_news
    FROM (
        SELECT i.id AS issue_id, i.reopened_at, i.resolved_at, i.newest_reply_at,
               i.thread_ref IS NOT NULL AND i.thread_gone_at IS NULL AS threaded,
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
    LEFT JOIN pr_review_verdicts tv ON tv.round_id = l.released_round_id AND tv.issue_id = l.issue_id
) n;
