-- One review model for every review hunter runs.
--
-- A PR review (035-038) and the review a fix gets before its PR opens are
-- the same thing: rounds of a reviewer over base..head, the issues each
-- round reported, and what each later round said about the earlier ones.
-- Those parts move to tables of their own, shared by every subject; what
-- only a PR review has -- the request, trust and watch on `pr_reviews`, a
-- round's binding and post, an issue's thread on the PR -- stays in PR
-- tables that extend the shared rows one to one. Existing PR reviews move
-- over with their ids: every review's id is its `pr_reviews` id, every
-- round's and issue's id the one it had, so `jobs.pr_review_id`, the
-- verdicts and the issue ids a reviewer was told keep naming the same rows.

-- A review of one subject: a PR hunter did not open ('pr'), a fix before
-- its PR opens ('fix'), or an engage's reply ('engage'). The owner names
-- what is reviewed: a PR review's request (`pr_reviews`, one review each),
-- or a finding and the job that began the chain under review (a fix, or
-- engage, chain: one review each; a finding fixed afresh is a new chain,
-- and a new review). The owner's columns point at it rather than the other
-- way round, so deleting the owner (a repo's `pr_reviews` rows go with the
-- repo) takes its review along, and a fix review needs no PR row.
CREATE TABLE reviews (
    id INTEGER PRIMARY KEY,
    subject TEXT NOT NULL CHECK (subject IN ('pr', 'fix', 'engage')),
    pr_review_id INTEGER UNIQUE REFERENCES pr_reviews(id) ON DELETE CASCADE,
    finding_id INTEGER REFERENCES findings(id) ON DELETE CASCADE,
    origin_job_id INTEGER UNIQUE REFERENCES jobs(id),
    CHECK (CASE subject
               WHEN 'pr' THEN pr_review_id IS NOT NULL AND finding_id IS NULL
                              AND origin_job_id IS NULL
               ELSE pr_review_id IS NULL AND finding_id IS NOT NULL
                    AND origin_job_id IS NOT NULL
           END)
);

-- One row per review attempt, whatever came of it (035's
-- `pr_review_rounds`, without what is a PR's).
CREATE TABLE review_rounds (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
    -- NULL when the round failed before a job existed (fetch, forge).
    job_id INTEGER REFERENCES jobs(id),
    -- The reviewed range base..head.
    base_sha TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    -- The merge-base of the head with the branch it merges into, as the
    -- round resolved it; NULL for a round that failed before resolving it.
    -- A round whose base is another commit reviewed incrementally.
    merge_base_sha TEXT,
    -- 'ok': the reviewer finished base..head, which is reviewed from now
    -- on. 'partial': it was stopped (its cap, the wall clock) after it
    -- wrote something; what it wrote counts, but base..head is not
    -- reviewed, and the next round reviews it again. Both are kept
    -- rounds. 'failed': nothing of the attempt counts.
    verdict TEXT NOT NULL CHECK (verdict IN ('ok', 'partial', 'failed')),
    -- Why a round failed or stopped partway.
    failure TEXT,
    finished_at INTEGER NOT NULL,
    -- Tokens the round's reviewer spent (input + output + cache writes);
    -- NULL where nobody counted them, as for every round before this.
    tokens_new INTEGER
);

-- A PR review round's binding and post (035's `binding`, 036's posting):
-- one row for every round of a PR review, failed ones too.
CREATE TABLE pr_review_posts (
    round_id INTEGER PRIMARY KEY REFERENCES review_rounds(id) ON DELETE CASCADE,
    -- The review's `binding` the round ran under, as its pick read it. A
    -- round of an earlier binding than the review's now is about another
    -- PR: kept as history, it moves nothing of the review and counts
    -- nowhere. Here and not on the round: only a PR review is bound to a
    -- URL, and the binding decides above all whether the round is posted.
    binding INTEGER NOT NULL,
    -- NULL for a failed round and for one with nothing to post, ever.
    -- Otherwise 'pending' (a user decides), 'approved' (the PR review
    -- duty's next run posts it), 'posted', or 'discarded' (a user decided
    -- never to, or the round is of an earlier binding).
    post_state TEXT CHECK (post_state IN ('pending', 'approved', 'posted', 'discarded')),
    -- As 036 describes them.
    post_error TEXT,
    post_attempts INTEGER NOT NULL DEFAULT 0,
    posted_at INTEGER,
    forge_review_ref TEXT,
    landed_at INTEGER,
    review_sent_at INTEGER
);

-- Issues a kept round reported, in the reviewer's output schema.
CREATE TABLE review_issues (
    id INTEGER PRIMARY KEY,
    round_id INTEGER NOT NULL REFERENCES review_rounds(id) ON DELETE CASCADE,
    -- introduced | incomplete | weak-proof | follow-up (free text: the
    -- reviewer's word is shown as given).
    kind TEXT NOT NULL,
    blocking INTEGER NOT NULL,
    severity TEXT,
    confidence REAL,
    file TEXT,
    line INTEGER,
    -- Which version of the file `line` numbers: 'RIGHT' the reviewed head,
    -- 'LEFT' the base, for an issue about code the change removes; NULL
    -- (the reviewer did not say) reads as 'RIGHT'.
    side TEXT CHECK (side IN ('RIGHT', 'LEFT')),
    summary TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT ''
);

-- A PR review issue's thread on the PR (036-038, which describe each
-- column): one row for every issue of a PR review, posted or not.
-- `replies_json` stays a column here, replaced by every read of the
-- thread, rather than rows of `review_answers`: it is the thread as the
-- newest read screened it, hunter's own replies included, not answers
-- recorded once.
CREATE TABLE pr_review_threads (
    issue_id INTEGER PRIMARY KEY REFERENCES review_issues(id) ON DELETE CASCADE,
    inline INTEGER NOT NULL DEFAULT 0,
    thread_ref TEXT,
    thread_gone_at INTEGER,
    answer TEXT CHECK (answer IN ('addressed', 'rebuttal-accepted', 'open')),
    resolved_at INTEGER,
    resolving INTEGER CHECK (resolving IN (0, 1)),
    reopened_at INTEGER,
    newest_reply_at INTEGER,
    answered_through INTEGER,
    replies_json TEXT NOT NULL DEFAULT '[]',
    hidden_replies INTEGER NOT NULL DEFAULT 0
);

-- What a kept round said about each earlier issue of its review. An issue
-- the round did not mention has no row: silence is no verdict, so it stays
-- as it was. A failed round records none.
--
-- The three `seen`/`answers` columns are what the round was shown of the
-- issue's thread on the PR (037, 038). They stay columns here rather than
-- a PR table of their own: each is NULL where the round was shown nothing
-- of the kind, which is what a review without threads always was shown.
CREATE TABLE review_verdicts (
    round_id INTEGER NOT NULL REFERENCES review_rounds(id) ON DELETE CASCADE,
    issue_id INTEGER NOT NULL REFERENCES review_issues(id) ON DELETE CASCADE,
    verdict TEXT NOT NULL CHECK (verdict IN ('addressed', 'rebuttal-accepted', 'open')),
    -- The reviewer's one sentence of evidence.
    note TEXT NOT NULL DEFAULT '',
    seen_reopened_at INTEGER,
    seen_reply_at INTEGER,
    answers_reply_at INTEGER,
    PRIMARY KEY (round_id, issue_id)
);
CREATE INDEX review_verdicts_issue ON review_verdicts(issue_id);

-- The other side's answer to an issue: the fixer's to a fix review's
-- round, or a human's. `round_id` is the round answered (NULL for an
-- answer to no round of the review); `answer` is whether the issue is
-- fixed (`commit_sha`, where) or argued away (`text`, why).
CREATE TABLE review_answers (
    id INTEGER PRIMARY KEY,
    issue_id INTEGER NOT NULL REFERENCES review_issues(id) ON DELETE CASCADE,
    round_id INTEGER REFERENCES review_rounds(id) ON DELETE CASCADE,
    author_kind TEXT NOT NULL CHECK (author_kind IN ('fixer', 'human')),
    author TEXT NOT NULL,
    answer TEXT NOT NULL CHECK (answer IN ('addressed', 'rebutted')),
    text TEXT NOT NULL DEFAULT '',
    commit_sha TEXT,
    at INTEGER NOT NULL
);

INSERT INTO reviews (id, subject, pr_review_id)
SELECT id, 'pr', id FROM pr_reviews;

INSERT INTO review_rounds
    (id, review_id, job_id, base_sha, head_sha, merge_base_sha, verdict, failure, finished_at)
SELECT id, review_id, job_id, base_sha, head_sha, merge_base_sha, verdict, failure, finished_at
FROM pr_review_rounds;

INSERT INTO pr_review_posts
    (round_id, binding, post_state, post_error, post_attempts, posted_at, forge_review_ref,
     landed_at, review_sent_at)
SELECT id, binding, post_state, post_error, post_attempts, posted_at, forge_review_ref,
       landed_at, review_sent_at
FROM pr_review_rounds;

INSERT INTO review_issues
    (id, round_id, kind, blocking, severity, confidence, file, line, side, summary, detail)
SELECT id, round_id, kind, blocking, severity, confidence, file, line, side, summary, detail
FROM pr_review_issues;

INSERT INTO pr_review_threads
    (issue_id, inline, thread_ref, thread_gone_at, answer, resolved_at, resolving, reopened_at,
     newest_reply_at, answered_through, replies_json, hidden_replies)
SELECT id, inline, thread_ref, thread_gone_at, answer, resolved_at, resolving, reopened_at,
       newest_reply_at, answered_through, replies_json, hidden_replies
FROM pr_review_issues;

INSERT INTO review_verdicts
    (round_id, issue_id, verdict, note, seen_reopened_at, seen_reply_at, answers_reply_at)
SELECT round_id, issue_id, verdict, note, seen_reopened_at, seen_reply_at, answers_reply_at
FROM pr_review_verdicts;

DROP VIEW pr_review_issue_state;
DROP TABLE pr_review_verdicts;
DROP TABLE pr_review_issues;
DROP TABLE pr_review_rounds;

-- 038's view over the new tables, unchanged: every PR review issue's
-- status, the round whose verdict its thread is to show, and the news in
-- its thread no verdict was given over yet. Only PR issues have a row: the
-- view reads their threads.
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
        SELECT t.issue_id, t.reopened_at, t.resolved_at, t.newest_reply_at,
               t.thread_ref IS NOT NULL AND t.thread_gone_at IS NULL AS threaded,
               (SELECT v.round_id FROM review_verdicts v
                JOIN review_rounds r ON r.id = v.round_id
                JOIN pr_review_posts o ON o.round_id = v.round_id
                WHERE v.issue_id = t.issue_id AND o.post_state IS NOT 'discarded'
                ORDER BY r.finished_at DESC, r.id DESC LIMIT 1) AS judged_round_id,
               (SELECT v.round_id FROM review_verdicts v
                JOIN review_rounds r ON r.id = v.round_id
                JOIN pr_review_posts o ON o.round_id = v.round_id
                WHERE v.issue_id = t.issue_id AND o.post_state IS NOT 'discarded'
                  AND (o.post_state IS NOT 'pending' OR o.landed_at IS NOT NULL)
                ORDER BY r.finished_at DESC, r.id DESC LIMIT 1) AS released_round_id
        FROM pr_review_threads t
    ) l
    LEFT JOIN review_verdicts jv ON jv.round_id = l.judged_round_id AND jv.issue_id = l.issue_id
    LEFT JOIN review_verdicts tv ON tv.round_id = l.released_round_id AND tv.issue_id = l.issue_id
) n;
