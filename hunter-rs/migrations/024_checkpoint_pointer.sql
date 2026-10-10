-- A finding's work continues from `finding_work.checkpoint_job_id`, not
-- from whichever of its job rows happens to read `suspended`
-- (WORK-MODEL.md §8). Job rows of finding kinds are no longer rewritten
-- once they finish: giving a chain up, finding its tree gone, or the
-- finding moving on clears the pointer and logs an event, where it used to
-- flip the row to `failed`/`killed`. Repo-level work keeps its suspended
-- rows as checkpoints (§13).

-- Which statuses each kind's work item is active in (§6.1). A harvest is
-- active only while its PR is also unharvested; that half lives in the
-- `pr_state` trigger below. `blocked` keeps a held fix's checkpoint until
-- holds replace that status.
CREATE TABLE work_tiers (
  kind   TEXT NOT NULL,
  status TEXT NOT NULL,
  PRIMARY KEY (kind, status)
);
INSERT INTO work_tiers (kind, status) VALUES
  ('recheck', 'rechecking'),
  ('fix', 'queued'),
  ('fix', 'fixing'),
  ('fix', 'blocked'),
  ('engage', 'pr_open'),
  ('harvest', 'merged'),
  ('harvest', 'closed');

-- Today's checkpoints: per finding and kind, the newest suspended attempt
-- nothing continued yet, while the finding is in that kind's tier. A
-- suspension outside its tier was never going to be resumed: the sweep
-- retired those as `finding-moved`.
INSERT INTO finding_work (finding_id, kind, checkpoint_job_id)
SELECT j.finding_id, j.kind, MAX(j.id)
FROM jobs j
JOIN findings f ON f.id = j.finding_id
JOIN work_tiers t ON t.kind = j.kind AND t.status = f.status
LEFT JOIN pr_state p ON p.finding_id = f.id
WHERE j.state = 'suspended'
  AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id)
  AND (j.kind <> 'harvest' OR p.harvested_at IS NULL)
GROUP BY j.finding_id, j.kind
ON CONFLICT (finding_id, kind) DO UPDATE SET checkpoint_job_id = excluded.checkpoint_job_id;

-- Leaving a tier ends the work item (§6.3): its checkpoint can never be
-- continued and its streak describes work that is over (G8). Done here,
-- in the transaction of whatever moved the finding, so no status writer
-- can forget it and nothing is left for a later sweep. A dropped
-- checkpoint is logged, as its retirement was.
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
END;

-- A harvested PR's harvest work item is over, whatever the finding's status.
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
END;
