-- The zero-token Renovate dependency scan runs on the admin lane
-- (`scheduler::dependency_scans`). When Renovate has no answer for a repo
-- (not installed, failed, looked nothing up), the AI `dep_update` scan has
-- to run instead, and that spends tokens, so it is the job lane's. This is
-- the request between the two: the admin lane records when it found no
-- answer, and the job lane's rotation offers the repo's `dep_update` only
-- while that request is newer than the repo's `last_dep_update_at`. A
-- successful AI scan moves `last_dep_update_at` past it, which answers it.
CREATE TABLE dep_scan_fallbacks (
  repo_id      INTEGER PRIMARY KEY REFERENCES repos(id) ON DELETE CASCADE,
  requested_at INTEGER NOT NULL
);
