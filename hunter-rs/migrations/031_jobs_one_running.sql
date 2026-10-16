-- At most one `running` job per finding, and per (repo, kind) for
-- repo-level work (WORK-MODEL.md §6.2, §10).
--
-- A finding is worked on by one attempt at a time, whatever the kind:
-- its status puts it in exactly one kind's tier, and a job outlives its
-- finding moving on (a merge seen while an engage runs), so the rule is
-- per finding, not per (finding, kind). Repo-level work has no finding;
-- one attempt per kind and repo. Until now the serial cycle kept both
-- true by never starting a second job; the indexes keep them true for
-- any caller, so an insert that would break them fails instead
-- (`Store::insert_job` reports the job holding the place).
--
-- Only `running` counts. A `suspended` row is history once a successor
-- continues it (migration 024): a resumed chain holds several, and what
-- is resumable lives in `finding_work`/`repo_work.checkpoint_job_id`.
--
-- These indexes are also what gives an attempt one successor at a time.
-- A resume or handoff names only its predecessor, and `insert_job` reads
-- the successor's place from that row (`JobStart`), so a second successor
-- would run on the same place and be refused here. No
-- UNIQUE(resumed_from): a successor orphaned before doing any work leaves
-- the checkpoint where it was, and that checkpoint may be continued again.
--
-- A database can already hold two `running` rows for one place: a runner
-- whose write ending its job failed leaves the row `running` until the
-- cycle settles it, and a crash before that settle leaves both for the
-- startup reconcile, which runs after this migration. Every such row is
-- a dead process's. The newest of each place is left to the reconcile;
-- the older ones are recorded as the reconcile records a killed orphan.
UPDATE jobs
SET state = 'killed',
    pid = NULL,
    killed_reason = 'orphaned',
    notes = 'reconciled by migration 031 -- a newer running job held the same place',
    finished_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE state = 'running'
  AND ((finding_id IS NOT NULL
        AND id < (SELECT MAX(d.id) FROM jobs d
                  WHERE d.state = 'running' AND d.finding_id = jobs.finding_id))
    OR (finding_id IS NULL
        AND id < (SELECT MAX(d.id) FROM jobs d
                  WHERE d.state = 'running' AND d.finding_id IS NULL
                    AND d.repo_id = jobs.repo_id AND d.kind = jobs.kind)));

CREATE UNIQUE INDEX jobs_one_running_per_finding
  ON jobs(finding_id) WHERE state = 'running' AND finding_id IS NOT NULL;
CREATE UNIQUE INDEX jobs_one_running_per_repo_kind
  ON jobs(repo_id, kind) WHERE state = 'running' AND finding_id IS NULL;
