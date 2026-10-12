-- Repo-level work items: one row per (repo, kind) that has state worth
-- keeping (WORK-MODEL.md §13). `kind` is a repo job kind, or `sync`: the
-- work every kind of the repo needs first (clone, fetch, resolving
-- `origin/<branch>`), whose failures break all of them at once.
--
-- `checkpoint_job_id` is the attempt the work continues from. A repo job's
-- suspended row used to be its checkpoint as long as nothing continued it,
-- and was rewritten `failed`/`killed` to stop being one. The pointer
-- replaces that, as `finding_work`'s did for finding work (migration 024):
-- no finished job row is rewritten any more. `sync` runs no worker, so it
-- never has one.
--
-- `streak` and `last_failure` are the identical-failure streak, as on
-- `finding_work`. A row is created by the first write that needs it, so a
-- missing row reads as "no streak, no checkpoint".
CREATE TABLE repo_work (
  repo_id           INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  kind              TEXT    NOT NULL CHECK (kind IN
                      ('hunt', 'test_gap', 'dep_update', 'refactor', 'modernization',
                       'standards', 'sync')),
  streak            INTEGER NOT NULL DEFAULT 0,
  last_failure      TEXT,
  checkpoint_job_id INTEGER REFERENCES jobs(id) ON DELETE SET NULL,
  PRIMARY KEY (repo_id, kind),
  CHECK (kind <> 'sync' OR checkpoint_job_id IS NULL)
);

-- Today's checkpoints: per repo and kind, the newest suspended attempt
-- nothing continued yet. Rows already retired (`give-up`, `workdir-gone`,
-- `superseded`) are no longer `suspended`, so they stay ended.
INSERT INTO repo_work (repo_id, kind, checkpoint_job_id)
SELECT j.repo_id, j.kind, MAX(j.id)
FROM jobs j
WHERE j.finding_id IS NULL
  AND j.kind IN ('hunt', 'test_gap', 'dep_update', 'refactor', 'modernization', 'standards')
  AND j.state = 'suspended'
  AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id)
GROUP BY j.repo_id, j.kind;
