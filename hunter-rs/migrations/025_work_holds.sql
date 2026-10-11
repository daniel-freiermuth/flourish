-- Work stopped for the operator, for every kind of finding work
-- (WORK-MODEL.md §9, §10, §11).
--
-- The active row (`released_at IS NULL`) is the hold; released rows are
-- the record of past holds and how each was released. Rows are only ever
-- appended; releasing fills in the `released_*` columns. At most one
-- active hold per (finding, kind), enforced by the partial index.
CREATE TABLE work_holds (
  id             INTEGER PRIMARY KEY,
  finding_id     INTEGER NOT NULL REFERENCES findings(id) ON DELETE CASCADE,
  kind           TEXT    NOT NULL CHECK (kind IN ('recheck', 'fix', 'engage', 'harvest')),
  reason         TEXT    NOT NULL CHECK (reason IN ('prerequisite', 'stuck', 'gave_up')),
  detail         TEXT    NOT NULL,
  held_at        INTEGER NOT NULL,
  released_at    INTEGER,
  released_by    TEXT,
  release_action TEXT    CHECK (release_action IN
                   ('continue', 'restart', 'reject', 'wontfix', 'classify',
                    'mark_harvested', 'dismiss')),
  CHECK ((released_at IS NULL) = (released_by IS NULL))
);
CREATE UNIQUE INDEX work_holds_one_active
  ON work_holds(finding_id, kind) WHERE released_at IS NULL;

-- Blocked fixes become holds (§11.4). The held job is the newest fix
-- attempt nothing continued that carries the report, as
-- `held_fix_blockers` read it. A report that starts with the failure
-- streak's prefix is `stuck`; anything else was a worker's BLOCKED.md.
INSERT INTO work_holds (finding_id, kind, reason, detail, held_at)
SELECT f.id, 'fix',
       CASE WHEN j.blocker GLOB 'stuck: [0-9]* consecutive fix attempts hit the same failure: *'
            THEN 'stuck' ELSE 'prerequisite' END,
       COALESCE(j.blocker, 'blocked before the upgrade; no report was kept'),
       COALESCE(j.finished_at, f.updated_at)
FROM findings f
LEFT JOIN jobs j ON j.id = (
  SELECT MAX(h.id) FROM jobs h
  WHERE h.finding_id = f.id AND h.kind = 'fix' AND h.state = 'suspended'
    AND h.blocker IS NOT NULL
    AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = h.id))
WHERE f.status = 'blocked';

-- Its checkpoint is the held job, but only if a worker ran in it: a hold
-- whose job never made a tree has nothing to continue (G9). Without one,
-- the pointer 024 backfilled stays if it names an attempt with a
-- transcript: the work done so far is still there to continue.
INSERT INTO finding_work (finding_id, kind, checkpoint_job_id)
SELECT f.id, 'fix', (
  SELECT MAX(h.id) FROM jobs h
  WHERE h.finding_id = f.id AND h.kind = 'fix' AND h.state = 'suspended'
    AND h.blocker IS NOT NULL AND h.session_file IS NOT NULL
    AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = h.id))
FROM findings f
WHERE f.status = 'blocked'
ON CONFLICT (finding_id, kind) DO UPDATE SET checkpoint_job_id = COALESCE(
  excluded.checkpoint_job_id,
  (SELECT j.id FROM jobs j
   WHERE j.id = finding_work.checkpoint_job_id AND j.session_file IS NOT NULL));

-- A held fix stays `queued` (§9.5, invariant 4); `blocked` is gone.
UPDATE findings SET status = 'queued' WHERE status = 'blocked';
DELETE FROM work_tiers WHERE kind = 'fix' AND status = 'blocked';

ALTER TABLE jobs DROP COLUMN blocker;

-- Leaving a tier now also releases the work item's active hold, as
-- `finding-left-tier`, in the same write (§9.5, invariant 2).
DROP TRIGGER findings_leave_work_tier;
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

DROP TRIGGER pr_state_harvested_ends_harvest_work;
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
