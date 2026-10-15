//! Every write to `pr_state`, and nothing else.
//!
//! The column groups and their writers are set out in `crate::attention`.
//! This module is where that split is enforced: the SQL that writes a
//! `pr_state` column exists only here (`check-sql.sh` fails the build on
//! one anywhere else under `src/`), each function writes one group, and
//! [`settle_attention`], the only writer of the derived columns, is private
//! to this module and called by every function that changes one of its
//! inputs. `Store` opens the transactions and calls these; it cannot write
//! a derived column, or an input without settling it, because the code
//! that would do so does not exist outside this file.

use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, PoisonError};

use super::{ImmediateTx, Store};
use crate::attention::{
    self, AnswerRecorded, AttentionChange, EngageAnswer, ObservedOpenPr, OpenPrRecorded,
};
use crate::domain::FindingStatus;
use crate::transition::{PrEnd, PrEnded, PrEnding, PrRef, Sources};
use crate::util::now_ms;

/// One pass of PR sync over a store: the only way to begin a [`PrRead`].
///
/// Observations are not ordered against each other, and need not be,
/// because no two reads of one PR are ever out at once. This type is what
/// makes that so:
///
/// * At most one pass per store is live at a time: [`Self::begin`] hands
///   out none while another is, and dropping the pass ends it.
/// * A pass reads each finding's PR at most once ([`Self::begin_read`]).
/// * Every [`PrRead`], and so every observation made from one, borrows its
///   pass, so none outlives the pass and lands after a later pass's.
///
/// So the last observation written of a PR is always the newest.
///
/// Begun only with proof of startup recovery (`Recovered::begin_pr_sync`,
/// `CycleScope::begin_pr_sync`). Outside a `test-support` build only the
/// daemon's store, opened under its `DaemonLock`, ever has that proof, so
/// no other process can sync alongside the daemon either.
pub struct PrSyncPass {
    store: Store,
    /// The findings whose PR this pass has begun a read of.
    read: Mutex<HashSet<i64>>,
}

impl PrSyncPass {
    /// Begin `store`'s pass, or `None` while another one is live.
    pub(crate) fn begin(store: &Store) -> Option<Self> {
        store
            .pr_sync
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()?;
        Some(Self {
            store: store.clone(),
            read: Mutex::default(),
        })
    }

    /// The store this pass syncs.
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Begin a forge read of `finding_id`'s open PR `pr`: the [`PrRead`]
    /// its observation must carry, taken now, before the read, with the
    /// row's push count as it stands now (migration 030).
    ///
    /// `None`: this pass has already begun a read of `finding_id`'s PR.
    pub async fn begin_read(
        &self,
        finding_id: i64,
        pr: &PrRef,
    ) -> sqlx::Result<Option<PrRead<'_>>> {
        self.read_at(finding_id, pr, now_ms()).await
    }

    /// [`Self::begin_read`] stamped `began_at` instead of now: a fixture,
    /// only in a `test-support` build. The push count is still the row's,
    /// and the once-per-pass rule still holds.
    #[cfg(feature = "test-support")]
    pub async fn begin_read_at(
        &self,
        finding_id: i64,
        pr: &PrRef,
        began_at: i64,
    ) -> sqlx::Result<Option<PrRead<'_>>> {
        self.read_at(finding_id, pr, began_at).await
    }

    /// Begin a read of `finding_id`'s open PR `pr`, stamped `began_at`. A
    /// finding with no row yet has had no push. A read that could not be
    /// begun was never out, so the finding stays readable.
    async fn read_at(
        &self,
        finding_id: i64,
        pr: &PrRef,
        began_at: i64,
    ) -> sqlx::Result<Option<PrRead<'_>>> {
        if !self.read_set().insert(finding_id) {
            return Ok(None);
        }
        let push_count = sqlx::query_scalar!(
            r#"SELECT push_count AS "push_count!: i64" FROM pr_state WHERE finding_id = ?1"#,
            finding_id
        )
        .fetch_optional(&self.store.pool)
        .await;
        let push_count = match push_count {
            Ok(count) => count.unwrap_or(0),
            Err(e) => {
                self.read_set().remove(&finding_id);
                return Err(e);
            }
        };
        Ok(Some(PrRead {
            began_at,
            pr: pr.clone(),
            push_count,
            _pass: PhantomData,
        }))
    }

    /// The lock is never held across an await, so a poisoned one only
    /// means a panic between two plain set operations: the set is intact.
    fn read_set(&self) -> std::sync::MutexGuard<'_, HashSet<i64>> {
        self.read.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for PrSyncPass {
    fn drop(&mut self) {
        self.store.pr_sync.store(false, Ordering::SeqCst);
    }
}

/// One forge read of one open PR, within one [`PrSyncPass`].
///
/// Begun before the read ([`PrSyncPass::begin_read`]), so its `began_at`
/// says when the forge was asked, not when it answered. It names the PR
/// read (`pr`, the finding's as sync listed it), so its observation lands
/// only while that is still the finding's PR, and records that PR's number.
/// Neither `Clone` nor `Copy`, and consumed by the observation it yields
/// ([`ObservedOpenPr::from_view`](crate::attention::ObservedOpenPr::from_view)):
/// one read, one observation. It borrows its pass (`'p`), so neither it
/// nor its observation outlives the pass.
///
/// It carries the row's `push_count` as it stood when the read began: an
/// engage that pushes after that counts one more, and the observation,
/// stored with the count it began at, is then known to predate the push
/// (migration 030).
#[derive(Debug, PartialEq, Eq)]
pub struct PrRead<'p> {
    /// Wall time the read began (epoch ms): the observation's `synced_at`.
    began_at: i64,
    /// The PR this read is of.
    pr: PrRef,
    /// The engage pushes recorded on the row when the read began.
    push_count: i64,
    _pass: PhantomData<&'p PrSyncPass>,
}

impl PrRead<'_> {
    /// The PR this read is of.
    pub const fn pr(&self) -> &PrRef {
        &self.pr
    }
}

/// Record what sync observed on an open PR (the observed group), and
/// settle its attention from that and the engage's last answer -- but only
/// while the finding is still `pr_open` with the PR that was read. `tx`
/// holds the write lock from its start ([`ImmediateTx`]), so nothing moves
/// the finding or the row between the check and the write.
///
/// A new row has no engagement watermark: a PR never engaged has answered
/// nothing, so every screened item on it is new (settle reads NULL as 0).
/// The watermark is engage's column; this writes none of it. An existing
/// row keeps whatever engage last recorded. A new `pr_number` clears the
/// harvest bookkeeping, as in [`record_end`].
///
/// Not `pr_open` ([`OpenPrRecorded::NotPrOpen`]): whatever moved the
/// finding recorded the PR's end, and this observation, older than that
/// end, must not reopen the row.
///
/// `pr_open` with another PR ([`OpenPrRecorded::OtherPr`]): the PR read
/// ended, and the finding shipped a new one, after the read began. The row
/// is the new PR's; the old PR's view, number included, must not land on
/// it. The status check alone does not catch this: the finding is
/// `pr_open` again, only with another PR, so the URL is checked too.
///
/// Nothing orders this observation against an earlier one, and nothing
/// needs to: its read borrows a [`PrSyncPass`], at most one of which is
/// live per store, and which reads each PR once.
///
/// The settle lands with the observation: `attention_since` is stamped only
/// on the write where the reasons *change*, so an observation committed
/// without it would leave every later settle seeing unchanged reasons, and
/// the attention age wrong for good.
pub(super) async fn record_open(
    tx: &mut ImmediateTx,
    finding_id: i64,
    pr: &ObservedOpenPr<'_>,
) -> sqlx::Result<OpenPrRecorded> {
    match standing(tx, finding_id, pr.read().pr.url()).await? {
        Standing::OnPr => {}
        Standing::NotPrOpen { now } => return Ok(OpenPrRecorded::NotPrOpen { now }),
        Standing::OtherPr => return Ok(OpenPrRecorded::OtherPr),
    }
    let state = crate::forge::PrState::Open.as_str();
    let mergeable = pr.mergeable().as_str();
    let failing_checks = pr.failing_checks_column();
    let read = pr.read();
    let (pr_number, changes_requested) = (pr.pr_number(), pr.changes_requested());
    let (checks, head_ref, head_sha) = (pr.checks(), pr.head_ref(), pr.head_sha());
    let last_activity_at = pr.last_activity_at();
    sqlx::query!(
        "INSERT INTO pr_state (finding_id, pr_number, state, mergeable, changes_requested, \
         checks, failing_checks, head_ref, head_sha, last_activity_at, synced_at, \
         observed_push_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
         ON CONFLICT(finding_id) DO UPDATE SET \
         pr_number = excluded.pr_number, \
         state = excluded.state, \
         mergeable = excluded.mergeable, \
         changes_requested = excluded.changes_requested, \
         checks = excluded.checks, \
         failing_checks = excluded.failing_checks, \
         head_ref = excluded.head_ref, \
         head_sha = excluded.head_sha, \
         last_activity_at = excluded.last_activity_at, \
         synced_at = excluded.synced_at, \
         observed_push_count = excluded.observed_push_count, \
         harvested_at = CASE WHEN pr_state.pr_number <> excluded.pr_number \
             THEN NULL ELSE pr_state.harvested_at END",
        finding_id,
        pr_number,
        state,
        mergeable,
        changes_requested,
        checks,
        failing_checks,
        head_ref,
        head_sha,
        last_activity_at,
        read.began_at,
        read.push_count
    )
    .execute(&mut **tx)
    .await?;
    let change = settle_attention(tx, finding_id, read.began_at).await?;
    Ok(OpenPrRecorded::Recorded(change))
}

/// End `finding_id`'s PR `pr` ([`PrEnding`]): the finding's move and the
/// end in `pr_state`, in one write, only while the finding is still
/// `pr_open` with that PR. The PR is checked under `tx`'s write lock, as an
/// observation's and an answer's are ([`standing`]), so nothing ships a
/// new PR between the check and the write.
///
/// The only way to record an end ([`record_end`] is private): no write
/// can end a PR its finding no longer has, or record an end without the
/// status move it belongs to.
pub(super) async fn end(
    tx: &mut ImmediateTx,
    finding_id: i64,
    pr: &PrRef,
    ending: PrEnding<'_>,
) -> sqlx::Result<PrEnded> {
    match standing(tx, finding_id, pr.url()).await? {
        Standing::NotPrOpen { now } => Ok(PrEnded::Conflict { now }),
        Standing::OtherPr => Ok(PrEnded::OtherPr),
        Standing::OnPr => {
            let now = now_ms();
            let from = Sources::One(ending.from());
            let (to, record) = (ending.to(), ending.record());
            if !Store::apply_edge(tx, finding_id, from, to, record, now).await? {
                // Unreachable while the lock holds: `standing` read
                // `pr_open` in this transaction.
                return Ok(PrEnded::Conflict {
                    now: super::status_now(&mut **tx, finding_id).await?,
                });
            }
            record_end(tx, finding_id, ending.end(), pr.number(), now).await?;
            Ok(PrEnded::Applied)
        }
    }
}

/// [`record_end`] with no check of the finding: a fixture write, only in a
/// `test-support` build, for a test that needs an ended PR's row under a
/// finding at any status.
#[cfg(feature = "test-support")]
pub(super) async fn record_end_unchecked(
    tx: &mut ImmediateTx,
    finding_id: i64,
    end: PrEnd,
    pr_number: i64,
    synced_at: i64,
) -> sqlx::Result<()> {
    record_end(tx, finding_id, end, pr_number, synced_at).await
}

/// Record that `finding_id`'s PR `pr_number` ended (UPSERT, the observed
/// group), and settle its attention, which an ended PR has none of. Only
/// [`end`] writes this in the daemon, with the status change it belongs
/// to, and only while the finding is still `pr_open` with the PR ended.
///
/// `pr_state` holds one row per finding, not per PR, and a finding can
/// ship more than one PR: a closure harvested as `abandoned` sends it
/// back to `new`, and its next fix opens a fresh PR on the same row.
/// The harvest bookkeeping (`harvested_at`, and the harvest streak in
/// `finding_work`, which migration 023's trigger resets) describes the
/// PR it was recorded for, so a different `pr_number` clears it;
/// otherwise the new PR inherits the old one's stamp and
/// `list_pending_harvest` never selects it. The same number keeps it,
/// so re-syncing a PR never un-harvests it. A stored NULL is an unknown
/// number, not another PR ([`fill_pr_number`] fills it in for the PR
/// already being worked), hence `<>` and not `IS NOT`. Every `pr_state`
/// upsert repeats this CASE: `query!` takes one literal, so the SQL
/// cannot be shared.
async fn record_end(
    tx: &mut ImmediateTx,
    finding_id: i64,
    end: PrEnd,
    pr_number: i64,
    synced_at: i64,
) -> sqlx::Result<()> {
    let state = end.as_str();
    sqlx::query!(
        "INSERT INTO pr_state (finding_id, pr_number, state, synced_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(finding_id) DO UPDATE SET \
         pr_number = excluded.pr_number, state = excluded.state, \
         synced_at = excluded.synced_at, \
         harvested_at = CASE WHEN pr_state.pr_number <> excluded.pr_number \
             THEN NULL ELSE pr_state.harvested_at END",
        finding_id,
        pr_number,
        state,
        synced_at
    )
    .execute(&mut **tx)
    .await?;
    settle_attention(tx, finding_id, synced_at).await?;
    Ok(())
}

/// Record an engage's answer (the answered group) and settle the PR's
/// attention from it -- but only while the finding is still `pr_open`
/// with the PR answered ([`AnswerRecorded`]), checked under `tx`'s write
/// lock as [`record_open`] checks an observation. The watermark only ever
/// rises. The decline is the answer's own, replacing the last answer's: a
/// push declines nothing. A push counts one more on the row, so every
/// observation from a read begun before it predates it ([`PrRead`]).
pub(super) async fn record_answer(
    tx: &mut ImmediateTx,
    finding_id: i64,
    answer: &EngageAnswer<'_>,
    now: i64,
) -> sqlx::Result<AnswerRecorded> {
    match standing(tx, finding_id, answer.pr().url()).await? {
        Standing::OnPr => {}
        Standing::NotPrOpen { now } => return Ok(AnswerRecorded::NotPrOpen { now }),
        Standing::OtherPr => return Ok(AnswerRecorded::OtherPr),
    }
    let seen = answer.seen().map(|s| s.0);
    let problems = answer.problems();
    let declined = problems.declined();
    let fingerprint = declined.map(|d| d.fingerprint());
    let head_sha = declined.map(|d| d.head_sha());
    let pushed = problems.pushed();
    // No mark (`None`) leaves the watermark exactly as it is, NULL
    // included: an answer that saw nothing answers nothing.
    sqlx::query!(
        "UPDATE pr_state SET \
         last_engaged_activity_at = CASE WHEN ?1 IS NULL THEN last_engaged_activity_at \
             ELSE MAX(COALESCE(last_engaged_activity_at, 0), ?1) END, \
         addressed_fingerprint = ?2, addressed_head_sha = ?3, \
         push_count = push_count + ?4 \
         WHERE finding_id = ?5",
        seen,
        fingerprint,
        head_sha,
        pushed,
        finding_id
    )
    .execute(&mut **tx)
    .await?;
    settle_attention(tx, finding_id, now)
        .await
        .map(AnswerRecorded::Recorded)
}

/// Where a finding stands against the PR at `pr_url` that a write is
/// about.
pub(super) enum Standing {
    /// `pr_open`, with that PR.
    OnPr,
    /// Not `pr_open` (`None`: no such finding).
    NotPrOpen { now: Option<FindingStatus> },
    /// `pr_open`, with another PR.
    OtherPr,
}

/// Where `finding_id` stands against the PR at `pr_url`, read under `tx`'s
/// write lock, so it holds until `tx` ends.
pub(super) async fn standing(
    tx: &mut ImmediateTx,
    finding_id: i64,
    pr_url: &str,
) -> sqlx::Result<Standing> {
    let finding = sqlx::query!(
        r#"SELECT status AS "status!: FindingStatus", pr_url FROM findings WHERE id = ?1"#,
        finding_id
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(match finding {
        Some(f) if f.status == FindingStatus::PrOpen => {
            if f.pr_url.as_deref() == Some(pr_url) {
                Standing::OnPr
            } else {
                Standing::OtherPr
            }
        }
        other => Standing::NotPrOpen {
            now: other.map(|f| f.status),
        },
    })
}

/// The operator's dismiss of a held engage, as a no-op engage's answer:
/// the newest feedback and the problems as they stand now are answered
/// (the answered group), and the attention is settled from that. A no-op
/// pushed nothing, so the push count stays.
pub(super) async fn record_dismiss(
    tx: &mut ImmediateTx,
    finding_id: i64,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE pr_state SET \
         last_engaged_activity_at = MAX(COALESCE(last_engaged_activity_at, 0), \
                                        COALESCE(last_activity_at, ?1)), \
         addressed_fingerprint = attention_fingerprint, \
         addressed_head_sha = head_sha \
         WHERE finding_id = ?2",
        now,
        finding_id
    )
    .execute(&mut **tx)
    .await?;
    settle_attention(tx, finding_id, now).await?;
    Ok(())
}

/// Stamp the PR harvested (the harvested group). No attention input.
pub(super) async fn stamp_harvested<'e, E: sqlx::SqliteExecutor<'e>>(
    ex: E,
    finding_id: i64,
    harvested_at: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE pr_state SET harvested_at = ?1 WHERE finding_id = ?2",
        harvested_at,
        finding_id
    )
    .execute(ex)
    .await?;
    Ok(())
}

/// Self-heal a missing `pr_number` (UPSERT -- row may not exist yet).
/// Fill-only: a number already recorded is never overwritten. The caller
/// read NULL, but a sync may have recorded the number since, and that
/// observation is the one that stands. Filling in a NULL leaves the
/// harvest bookkeeping alone: it is the same PR. No attention input
/// changes, so there is nothing to settle.
pub(super) async fn fill_pr_number<'e, E: sqlx::SqliteExecutor<'e>>(
    ex: E,
    finding_id: i64,
    pr_number: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO pr_state (finding_id, pr_number) VALUES (?1, ?2) \
         ON CONFLICT(finding_id) DO UPDATE SET \
         pr_number = COALESCE(pr_state.pr_number, excluded.pr_number)",
        finding_id,
        pr_number
    )
    .execute(ex)
    .await?;
    Ok(())
}

/// Settle `finding_id`'s derived attention columns (`needs_attention`,
/// `attention_since`) from its `pr_state` row as it stands in `tx`, and
/// void a decline that no longer describes the PR ([`attention::settle`]).
///
/// Private: every write here to the observed or the answered columns calls
/// this in its own transaction, after its write, and nothing else can
/// write the derived ones, so they cannot disagree with their inputs.
/// `attention_since` moves only when the reasons change, so a reason that
/// stays outstanding keeps its age: it is `list_attention`'s fairness key.
async fn settle_attention(
    tx: &mut ImmediateTx,
    finding_id: i64,
    now: i64,
) -> sqlx::Result<AttentionChange> {
    let open = crate::forge::PrState::Open.as_str();
    let conflicting = crate::forge::Mergeable::Conflicting.as_str();
    let Some(row) = sqlx::query!(
        r#"SELECT COALESCE(state = ?1, 0) AS "open!: bool",
                  COALESCE(last_activity_at, 0) AS "last_activity_at!: i64",
                  changes_requested AS "changes_requested!: bool",
                  COALESCE(mergeable = ?2, 0) AS "conflicting!: bool",
                  failing_checks IS NOT NULL AS "checks_failing!: bool",
                  attention_fingerprint AS "attention_fingerprint?: String", head_sha,
                  COALESCE(last_engaged_activity_at, 0) AS "engaged_at!: i64",
                  addressed_fingerprint, addressed_head_sha, needs_attention,
                  observed_push_count < push_count AS "predates_push!: bool"
           FROM pr_state WHERE finding_id = ?3"#,
        open,
        conflicting,
        finding_id
    )
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(AttentionChange::default());
    };
    let settled = attention::settle(&attention::Inputs {
        open: row.open,
        last_activity_at: row.last_activity_at,
        problems: attention::Problems {
            changes_requested: row.changes_requested,
            conflicting: row.conflicting,
            checks_failing: row.checks_failing,
        },
        fingerprint: row.attention_fingerprint,
        head_sha: row.head_sha,
        engaged_at: row.engaged_at,
        addressed_fingerprint: row.addressed_fingerprint,
        addressed_head_sha: row.addressed_head_sha,
        predates_push: row.predates_push,
    });
    if settled.void_addressed {
        sqlx::query!(
            "UPDATE pr_state SET addressed_fingerprint = NULL, addressed_head_sha = NULL \
             WHERE finding_id = ?1",
            finding_id
        )
        .execute(&mut **tx)
        .await?;
    }
    if settled.attention != row.needs_attention {
        let since = settled.attention.as_ref().map(|_| now);
        sqlx::query!(
            "UPDATE pr_state SET needs_attention = ?1, attention_since = ?2 \
             WHERE finding_id = ?3",
            settled.attention,
            since,
            finding_id
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(AttentionChange {
        before: row.needs_attention,
        now: settled.attention,
    })
}
