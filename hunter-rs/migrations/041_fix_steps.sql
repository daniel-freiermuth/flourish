-- A fix job resumes at its step (WORK-MODEL.md §5.7).
--
-- A fix attempt runs a fixer, review rounds, the fixer's answers, a
-- curator and the publish, all in one job. Where it got to was kept in
-- five files in the chain's workspace and read back from them and from
-- the tree; it is now one record on the work item.
--
-- `finding_work.step`: for a `fix` work item, a JSON record of the step
-- its next attempt runs (`step`: `implement`, `review N`, `answer N`,
-- `curate` or `publish`) and what the steps keep between attempts: the PR
-- title, the review's follow-ups, the verdict that ships and the tree it
-- covers, and a tree an agent may have changed, to put back before anything
-- runs (`scheduler::review::StepRecord`). NULL: the work starts cold, at
-- `implement`. Written with the checkpoint, in the transaction that ends an
-- attempt, and as an attempt moves from one step to the next.
--
-- `jobs.ended_step`: the `step` of the record an attempt ended with. An
-- attempt that ended at the step its predecessor ended at made no progress;
-- only those count toward the resume ceiling's attempts
-- (`Store::resume_chain_stats`).
ALTER TABLE finding_work ADD COLUMN step TEXT;
ALTER TABLE jobs ADD COLUMN ended_step TEXT;

-- The step is where the checkpoint continues from, so it goes with it:
-- whatever clears the checkpoint (an attempt that ended without pausing, a
-- restart, a given-up chain, the finding leaving its tier) clears the step
-- in the same statement's transaction, and the next attempt starts cold.
CREATE TRIGGER finding_work_step_follows_checkpoint
AFTER UPDATE OF checkpoint_job_id ON finding_work
WHEN NEW.checkpoint_job_id IS NULL AND NEW.step IS NOT NULL
BEGIN
  UPDATE finding_work SET step = NULL
  WHERE finding_id = NEW.finding_id AND kind = NEW.kind;
END;
