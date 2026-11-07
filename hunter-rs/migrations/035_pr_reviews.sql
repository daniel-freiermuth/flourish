-- Reviews of pull requests hunter did not open (future.md §8.8, slice 1).
--
-- A review is requested by hand for one PR. With `watch` on, every new head
-- the PR settles on is reviewed again, incrementally from the last reviewed
-- commit; with it off only the first round runs. Results live here, not in
-- findings: a reviewed PR is somebody else's change, so its issues are notes
-- about that change and never enter hunter's fix pipeline.
CREATE TABLE pr_reviews (
    id INTEGER PRIMARY KEY,
    repo_id INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    pr_number INTEGER NOT NULL,
    -- The repo's URL when the review was requested: the one the request's
    -- trust decision (the author can push there, or a user vouched for
    -- running their code) was made against, and whose PR `pr_number` is.
    -- While the repo's URL is another, the number may name another PR, by
    -- an author nobody decided about: the review does nothing until a
    -- request binds it to the new URL, deciding afresh.
    repo_url TEXT NOT NULL,
    -- Which binding to a URL the review is in: 1 for its first request,
    -- one more for each request that bound it to a new URL. A rebind
    -- starts the review afresh: what a round learned was about the PR its
    -- binding's URL named, so only the current binding's rounds are the
    -- review's, and nothing of an earlier one's counts again.
    binding INTEGER NOT NULL DEFAULT 1,
    -- Refreshed by the PR review duty; shown in the UI and the job label.
    title TEXT NOT NULL DEFAULT '',
    -- Forge login of the PR's author at request time.
    author TEXT NOT NULL,
    -- Branch the PR merges into; the first round's base is its merge-base
    -- with the head.
    base_ref TEXT NOT NULL,
    -- 1 when the head lives in a fork: vouching for the author then also
    -- vouches for whoever can push to that fork.
    cross_repo INTEGER NOT NULL DEFAULT 0,
    -- 1: review every new head once it is stable; 0: only the round a
    -- request asks for.
    watch INTEGER NOT NULL,
    -- Only an 'open' review is worked on. The sync sets 'merged', and
    -- 'closed', which a request that finds the PR reopened undoes; 'own'
    -- ends a review whose PR turned out to be one hunter opened. 'merged'
    -- and 'own' are final for the review's current binding: a request that
    -- binds it to the repo's new URL opens it again, its number naming
    -- another PR there.
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'merged', 'closed', 'own')),
    -- Newest head the forge reported, and when it was first seen. A watched
    -- review waits for the head to stay unchanged a while so a burst of
    -- pushes is reviewed once.
    head_sha TEXT,
    head_seen_at INTEGER,
    -- Head of the newest round that finished reviewing (verdict 'ok'); NULL
    -- until one did, and again once a request bound the review to a new
    -- URL. The next round reviews from it.
    last_reviewed_sha TEXT,
    -- 1 while a request (the first, one that reopens the review, or one
    -- that binds it to a new URL) waits for a finished round: due at once,
    -- watched or not, while the head is not the reviewed one. A finished
    -- round clears it.
    round_requested INTEGER NOT NULL DEFAULT 1 CHECK (round_requested IN (0, 1)),
    -- The author, when they cannot push and a user vouched for running
    -- their code; NULL for trusted authors.
    vouched_author TEXT,
    requested_by TEXT NOT NULL,
    requested_at INTEGER NOT NULL,
    -- Consecutive rounds that did not finish ('failed' or 'partial') and
    -- the newest reason. At 3 the review is held until a user retries or
    -- turns watch on.
    failures INTEGER NOT NULL DEFAULT 0,
    last_failure TEXT,
    held INTEGER NOT NULL DEFAULT 0,
    UNIQUE (repo_id, pr_number)
);

-- One row per review attempt, whatever came of it.
CREATE TABLE pr_review_rounds (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL REFERENCES pr_reviews(id) ON DELETE CASCADE,
    -- The review's `binding` the round ran under, as its pick read it. A
    -- round of an earlier binding than the review's now is about another
    -- PR: kept as history, it moves nothing of the review and counts
    -- nowhere.
    binding INTEGER NOT NULL,
    -- NULL when the round failed before a job existed (fetch, forge).
    job_id INTEGER REFERENCES jobs(id),
    -- The reviewed range base..head.
    base_sha TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    -- 'ok': the reviewer finished base..head, which is reviewed from now
    -- on. 'partial': it was stopped (its cap, the wall clock) after it
    -- wrote something; what it wrote counts, but base..head is not
    -- reviewed, and the next round reviews it again. Both are kept
    -- rounds. 'failed': nothing of the attempt counts.
    verdict TEXT NOT NULL CHECK (verdict IN ('ok', 'partial', 'failed')),
    -- Why a round failed or stopped partway.
    failure TEXT,
    finished_at INTEGER NOT NULL
);

-- Issues a kept round reported, in the reviewer's output schema.
CREATE TABLE pr_review_issues (
    id INTEGER PRIMARY KEY,
    round_id INTEGER NOT NULL REFERENCES pr_review_rounds(id) ON DELETE CASCADE,
    -- introduced | incomplete | weak-proof | follow-up (free text: the
    -- reviewer's word is shown as given).
    kind TEXT NOT NULL,
    blocking INTEGER NOT NULL,
    severity TEXT,
    confidence REAL,
    file TEXT,
    line INTEGER,
    summary TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT ''
);

-- The review a pr_review job ran for. NULL for every other kind, and for
-- every row written before this column existed.
ALTER TABLE jobs ADD COLUMN pr_review_id INTEGER REFERENCES pr_reviews(id);
