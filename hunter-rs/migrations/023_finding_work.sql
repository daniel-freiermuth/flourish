-- Per-finding work items: one row per (finding, kind) that has state worth
-- keeping (WORK-MODEL.md §10).
--
-- `streak` and `last_failure` are the identical-failure streak of that
-- work: how many attempts in a row failed with the key `last_failure`.
-- They used to live in three places, one pair per kind that had a streak
-- (`findings.fix_attempts`/`last_fix_failure`,
-- `findings.recheck_attempts`/`last_recheck_failure`,
-- `pr_state.harvest_attempts`/`last_harvest_failure`). They are copied
-- here and dropped there.
--
-- `checkpoint_job_id` is the attempt the work continues from. Nothing
-- sets it yet; the checkpoint still lives on the `suspended` job rows.
--
-- A row is created by the first write that needs it, so a missing row
-- reads as "no streak, no checkpoint".
CREATE TABLE finding_work (
  finding_id        INTEGER NOT NULL REFERENCES findings(id) ON DELETE CASCADE,
  kind              TEXT    NOT NULL CHECK (kind IN ('recheck', 'fix', 'engage', 'harvest')),
  streak            INTEGER NOT NULL DEFAULT 0,
  last_failure      TEXT,
  checkpoint_job_id INTEGER REFERENCES jobs(id) ON DELETE SET NULL,
  PRIMARY KEY (finding_id, kind)
);

INSERT INTO finding_work (finding_id, kind, streak, last_failure)
SELECT id, 'fix', fix_attempts, last_fix_failure FROM findings
WHERE fix_attempts <> 0 OR last_fix_failure IS NOT NULL;

INSERT INTO finding_work (finding_id, kind, streak, last_failure)
SELECT id, 'recheck', recheck_attempts, last_recheck_failure FROM findings
WHERE recheck_attempts <> 0 OR last_recheck_failure IS NOT NULL;

INSERT INTO finding_work (finding_id, kind, streak, last_failure)
SELECT finding_id, 'harvest', harvest_attempts, last_harvest_failure FROM pr_state
WHERE harvest_attempts <> 0 OR last_harvest_failure IS NOT NULL;

ALTER TABLE findings DROP COLUMN fix_attempts;
ALTER TABLE findings DROP COLUMN last_fix_failure;
ALTER TABLE findings DROP COLUMN recheck_attempts;
ALTER TABLE findings DROP COLUMN last_recheck_failure;
ALTER TABLE pr_state DROP COLUMN harvest_attempts;
ALTER TABLE pr_state DROP COLUMN last_harvest_failure;

-- A finding can ship more than one PR on its one `pr_state` row (a closure
-- harvested as `abandoned` sends it back to triage, and its next fix opens
-- a new PR). The harvest streak describes the PR it was counted for, so a
-- different `pr_number` resets it, as the upserts reset `harvested_at`. A
-- trigger rather than a clause in each of the four upserts: the old
-- columns had to be repeated in every one, and a fifth would forget it.
-- A NULL number is an unknown one being filled in, not another PR.
CREATE TRIGGER pr_state_new_pr_resets_harvest_streak
AFTER UPDATE OF pr_number ON pr_state
WHEN OLD.pr_number <> NEW.pr_number
BEGIN
  UPDATE finding_work SET streak = 0, last_failure = NULL
  WHERE finding_id = NEW.finding_id AND kind = 'harvest';
END;
