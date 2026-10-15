-- The static problems PR sync observed, stored as observations so that
-- `needs_attention` can be derived from the row alone (src/attention.rs).
--
-- Before this, sync computed `needs_attention` itself, from a copy of the
-- row it read before its write -- including the engage columns
-- (`last_engaged_activity_at`, `addressed_*`), which it wrote back. An
-- engage that recorded its answer between that read and that write was
-- undone: the old watermark came back and the answered comments were
-- flagged again. Now sync writes only what it observed, and
-- `needs_attention`/`attention_since` are settled from the stored columns
-- inside the transaction of every write that feeds them.
--
-- Each problem is stored once. `mergeable` already records a conflict;
-- `changes_requested` and `failing_checks` (the failing checks' names,
-- sorted, unique, comma-joined; NULL when none fail) record the other two.
-- `attention_fingerprint`, the signature an engage's decline is matched
-- against, becomes a column SQLite computes from those three: it can no
-- longer disagree with them, and nothing can write it.
--
-- The backfill reads the two new columns out of the old fingerprint, whose
-- format puts them in fixed places: the review part first, the checks part
-- last. The next sync rewrites both from the forge anyway.
ALTER TABLE pr_state ADD COLUMN changes_requested INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pr_state ADD COLUMN failing_checks TEXT;

UPDATE pr_state SET
  changes_requested = instr(COALESCE(attention_fingerprint, ''), 'review:CHANGES_REQUESTED') = 1,
  failing_checks = CASE
    WHEN instr(COALESCE(attention_fingerprint, ''), 'checks:') > 0
    THEN substr(attention_fingerprint, instr(attention_fingerprint, 'checks:') + 7)
  END;

ALTER TABLE pr_state DROP COLUMN attention_fingerprint;

-- `review:CHANGES_REQUESTED|mergeable:CONFLICTING|checks:a,b`, each part
-- only when present; NULL when the PR has none of the three.
ALTER TABLE pr_state ADD COLUMN attention_fingerprint TEXT GENERATED ALWAYS AS (
  NULLIF(substr(
    CASE WHEN changes_requested THEN '|review:CHANGES_REQUESTED' ELSE '' END
    || CASE WHEN mergeable = 'CONFLICTING' THEN '|mergeable:CONFLICTING' ELSE '' END
    || CASE WHEN failing_checks IS NOT NULL THEN '|checks:' || failing_checks ELSE '' END,
    2), '')
) VIRTUAL;
