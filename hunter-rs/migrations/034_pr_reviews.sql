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
    -- Refreshed by the PR sync; shown in the UI and the job label.
    title TEXT NOT NULL DEFAULT '',
    -- Forge login of the PR's author at request time.
    author TEXT NOT NULL,
    -- Branch the PR merges into; the first round's base is its merge-base
    -- with the head.
    base_ref TEXT NOT NULL,
    -- 1 when the head lives in a fork: vouching for the author then also
    -- vouches for whoever can push to that fork.
    cross_repo INTEGER NOT NULL DEFAULT 0,
    -- 1: review every new head once it is stable; 0: first round only.
    watch INTEGER NOT NULL,
    -- 'merged' and 'closed' are terminal; the sync sets them.
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'merged', 'closed')),
    -- Newest head the forge reported, and when it was first seen. A watched
    -- review waits for the head to stay unchanged a while so a burst of
    -- pushes is reviewed once.
    head_sha TEXT,
    head_seen_at INTEGER,
    -- Head of the newest successful round; NULL until one succeeded.
    last_reviewed_sha TEXT,
    -- The author, when they cannot push and a user vouched for running
    -- their code; NULL for trusted authors.
    vouched_author TEXT,
    requested_by TEXT NOT NULL,
    requested_at INTEGER NOT NULL,
    -- Consecutive failed rounds and the newest reason. At 3 the review is
    -- held until a user retries or turns watch on.
    failures INTEGER NOT NULL DEFAULT 0,
    last_failure TEXT,
    held INTEGER NOT NULL DEFAULT 0,
    UNIQUE (repo_id, pr_number)
);

-- One row per review attempt, successful or not.
CREATE TABLE pr_review_rounds (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL REFERENCES pr_reviews(id) ON DELETE CASCADE,
    -- NULL when the round failed before a job existed (fetch, forge).
    job_id INTEGER REFERENCES jobs(id),
    -- The reviewed range base..head.
    base_sha TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    verdict TEXT NOT NULL CHECK (verdict IN ('ok', 'failed')),
    failure TEXT,
    finished_at INTEGER NOT NULL
);

-- Issues a successful round reported, in the reviewer's output schema.
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
