-- A sixth scan kind, `simplification`: complexity whose reason is gone
-- (playbooks/simplification.md). `repos` gets its rotation stamp, and the
-- two tables whose CHECK lists the repo kinds learn the new one.
--
-- SQLite cannot change a CHECK in place, so both tables are rebuilt, as
-- migration 027 rebuilt `work_holds`: the two triggers that release
-- finding holds name it, and a rename re-parses every trigger, so they
-- are dropped first and recreated unchanged afterwards. Ids are kept.
ALTER TABLE repos ADD COLUMN last_simplification_at INTEGER;

DROP TRIGGER findings_leave_work_tier;
DROP TRIGGER pr_state_harvested_ends_harvest_work;

CREATE TABLE repo_work_new (
  repo_id           INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  kind              TEXT    NOT NULL CHECK (kind IN
                      ('hunt', 'test_gap', 'dep_update', 'refactor', 'modernization',
                       'standards', 'simplification', 'sync')),
  streak            INTEGER NOT NULL DEFAULT 0,
  last_failure      TEXT,
  checkpoint_job_id INTEGER REFERENCES jobs(id) ON DELETE SET NULL,
  PRIMARY KEY (repo_id, kind),
  CHECK (kind <> 'sync' OR checkpoint_job_id IS NULL)
);
INSERT INTO repo_work_new (repo_id, kind, streak, last_failure, checkpoint_job_id)
SELECT repo_id, kind, streak, last_failure, checkpoint_job_id FROM repo_work;
DROP TABLE repo_work;
ALTER TABLE repo_work_new RENAME TO repo_work;

CREATE TABLE work_holds_new (
  id             INTEGER PRIMARY KEY,
  finding_id     INTEGER REFERENCES findings(id) ON DELETE CASCADE,
  repo_id        INTEGER REFERENCES repos(id) ON DELETE CASCADE,
  kind           TEXT    NOT NULL,
  reason         TEXT    NOT NULL CHECK (reason IN ('prerequisite', 'stuck', 'gave_up')),
  detail         TEXT    NOT NULL,
  held_at        INTEGER NOT NULL,
  released_at    INTEGER,
  released_by    TEXT,
  release_action TEXT    CHECK (release_action IN
                   ('continue', 'restart', 'reject', 'wontfix', 'classify',
                    'mark_harvested', 'dismiss')),
  CHECK ((released_at IS NULL) = (released_by IS NULL)),
  CHECK ((finding_id IS NULL) <> (repo_id IS NULL)),
  CHECK (CASE WHEN finding_id IS NOT NULL
              THEN kind IN ('recheck', 'fix', 'engage', 'harvest')
              ELSE kind IN ('hunt', 'test_gap', 'dep_update', 'refactor', 'modernization',
                            'standards', 'simplification', 'sync') END)
);
INSERT INTO work_holds_new
  (id, finding_id, repo_id, kind, reason, detail, held_at, released_at, released_by,
   release_action)
SELECT id, finding_id, repo_id, kind, reason, detail, held_at, released_at, released_by,
       release_action
FROM work_holds;
DROP TABLE work_holds;
ALTER TABLE work_holds_new RENAME TO work_holds;

-- At most one active hold per subject and kind.
CREATE UNIQUE INDEX work_holds_one_active
  ON work_holds(finding_id, kind) WHERE released_at IS NULL;
CREATE UNIQUE INDEX work_holds_one_active_repo
  ON work_holds(repo_id, kind) WHERE released_at IS NULL;

-- As migration 027 left them.
CREATE TRIGGER findings_leave_work_tier
AFTER UPDATE OF status ON findings
WHEN OLD.status IS NOT NEW.status
BEGIN
  INSERT INTO events (at, kind, message, job_id, finding_id)
  SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER), 'resume',
         'resume ' || w.kind || ': checkpoint job ' || w.checkpoint_job_id
           || ' dropped, finding #' || NEW.id || ' is ' || NEW.status,
         w.checkpoint_job_id, NEW.id
  FROM finding_work w
  WHERE w.finding_id = NEW.id AND w.checkpoint_job_id IS NOT NULL
    AND EXISTS (SELECT 1 FROM work_tiers t WHERE t.kind = w.kind AND t.status = OLD.status)
    AND NOT EXISTS (SELECT 1 FROM work_tiers t WHERE t.kind = w.kind AND t.status = NEW.status);
  UPDATE finding_work SET streak = 0, last_failure = NULL, checkpoint_job_id = NULL
  WHERE finding_id = NEW.id
    AND EXISTS (SELECT 1 FROM work_tiers t WHERE t.kind = finding_work.kind AND t.status = OLD.status)
    AND NOT EXISTS (SELECT 1 FROM work_tiers t WHERE t.kind = finding_work.kind AND t.status = NEW.status);
  INSERT INTO events (at, kind, message, finding_id)
  SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER), 'release',
         '#' || NEW.id || ' ' || h.kind || ' hold released: finding is ' || NEW.status,
         NEW.id
  FROM work_holds h
  WHERE h.finding_id = NEW.id AND h.released_at IS NULL
    AND EXISTS (SELECT 1 FROM work_tiers t WHERE t.kind = h.kind AND t.status = OLD.status)
    AND NOT EXISTS (SELECT 1 FROM work_tiers t WHERE t.kind = h.kind AND t.status = NEW.status);
  UPDATE work_holds
  SET released_at = CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
      released_by = 'finding-left-tier'
  WHERE finding_id = NEW.id AND released_at IS NULL
    AND EXISTS (SELECT 1 FROM work_tiers t WHERE t.kind = work_holds.kind AND t.status = OLD.status)
    AND NOT EXISTS (SELECT 1 FROM work_tiers t WHERE t.kind = work_holds.kind AND t.status = NEW.status);
END;

CREATE TRIGGER pr_state_harvested_ends_harvest_work
AFTER UPDATE OF harvested_at ON pr_state
WHEN OLD.harvested_at IS NULL AND NEW.harvested_at IS NOT NULL
BEGIN
  INSERT INTO events (at, kind, message, job_id, finding_id)
  SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER), 'resume',
         'resume harvest: checkpoint job ' || w.checkpoint_job_id
           || ' dropped, finding #' || NEW.finding_id || ' is harvested',
         w.checkpoint_job_id, NEW.finding_id
  FROM finding_work w
  WHERE w.finding_id = NEW.finding_id AND w.kind = 'harvest'
    AND w.checkpoint_job_id IS NOT NULL;
  UPDATE finding_work SET streak = 0, last_failure = NULL, checkpoint_job_id = NULL
  WHERE finding_id = NEW.finding_id AND kind = 'harvest';
  INSERT INTO events (at, kind, message, finding_id)
  SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER), 'release',
         '#' || NEW.finding_id || ' harvest hold released: PR is harvested',
         NEW.finding_id
  FROM work_holds h
  WHERE h.finding_id = NEW.finding_id AND h.kind = 'harvest' AND h.released_at IS NULL;
  UPDATE work_holds
  SET released_at = CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
      released_by = 'finding-left-tier'
  WHERE finding_id = NEW.finding_id AND kind = 'harvest' AND released_at IS NULL;
END;
