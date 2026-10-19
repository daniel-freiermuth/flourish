-- The static problems and head an engage chain started from: the
-- pr_state row's attention_fingerprint and head_sha when its first attempt
-- read it.
--
-- Set by the chain's first attempt. A resumed attempt copies them from the
-- attempt it continues at INSERT, as with seen_activity_at: a resume is
-- prompted only to carry on, in the tree created at the first attempt's
-- head, so problems a sync recorded while the chain was suspended never
-- reached its worker. A reply without a push declines the row's problems
-- only while they are still these; declining the row as it stood when the
-- resume began hid a conflict no worker was shown.
--
-- NULL for every non-engage job, for a first attempt whose row recorded no
-- problems or no head, and for every row written before these columns
-- existed; such a chain declines nothing on a resume.
ALTER TABLE jobs ADD COLUMN started_on_fingerprint TEXT;
ALTER TABLE jobs ADD COLUMN started_on_head_sha TEXT;
