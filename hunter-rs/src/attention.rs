//! What an open PR needs from an engage, derived from what is known about
//! it. Nothing writes `pr_state.needs_attention` by hand.
//!
//! `pr_state`'s columns fall into groups by writer, and each group has
//! only its own writers:
//!
//! * **Observed**: `pr_number`, `state`, `mergeable`, `changes_requested`,
//!   `checks`, `failing_checks`, `head_ref`, `head_sha`,
//!   `last_activity_at`, `synced_at`, `observed_push_count`. An open PR
//!   is recorded by PR sync alone ([`ObservedOpenPr`], made only from a
//!   forge read of one PR, through `Store::record_open_pr`), and only
//!   while its finding is still `pr_open` with that PR
//!   ([`OpenPrRecorded::NotPrOpen`] and [`OpenPrRecorded::OtherPr`]
//!   otherwise). Observations are not ordered against each other, and
//!   need not be: every read belongs to a
//!   [`PrSyncPass`](crate::store::PrSyncPass), at most one of which is
//!   live per store and which reads each PR once, so no two reads of one
//!   PR are ever out at once. They are ordered against an engage's pushes:
//!   a read takes the row's `push_count` when it begins, and the
//!   observation keeps it ([`Inputs::predates_push`]). A PR's end is
//!   recorded with its finding's move in `Store::end_pr`, by sync
//!   (merged, closed on the forge) or by an engage withdrawing the PR,
//!   and, like an observation, only while the finding is still `pr_open`
//!   with that PR (`PrEnded::OtherPr` otherwise).
//!   One more writer: `Store::fill_pr_number`, which fills in a
//!   `pr_number` that is NULL and never overwrites one.
//! * **Answered**, written by engage (`Store::mark_pr_engaged`, and the
//!   operator's dismiss of a held engage, which counts as a no-op engage):
//!   `last_engaged_activity_at`, `addressed_fingerprint`,
//!   `addressed_head_sha`, `push_count`. An engage's answer names the PR
//!   it answered ([`EngageAnswer::pr`]) and lands only while the
//!   finding is still `pr_open` with that PR ([`AnswerRecorded`]), as an
//!   observation does. A pushed answer counts one more push. The one
//!   other writer is settle, which only clears a decline that no longer
//!   matches the PR (below).
//! * **Harvested**, written by harvest (and the operator's
//!   `mark_harvested` of a held one): `harvested_at`. Every write that
//!   moves the row to another `pr_number` clears it: the stamp is the old
//!   PR's.
//! * **Derived**: `needs_attention` and `attention_since`, written only by
//!   the store's settle step from [`settle`]. Settle runs inside the
//!   transaction of every write to the observed and answered groups that
//!   can change an input of [`settle`].
//! * **Computed by SQLite**: `attention_fingerprint`, the signature of the
//!   PR's static problems that a decline is matched against
//!   (`review:CHANGES_REQUESTED|mergeable:CONFLICTING|checks:a,b`, each
//!   part only when present; NULL when there are none). A generated column
//!   over `changes_requested`, `mergeable` and `failing_checks` (migration
//!   029): each problem is stored once, and nothing can write the
//!   fingerprint, so it cannot disagree with them.
//!
//! Every one of those writes is in `store/pr_state.rs`, one function per
//! group, with the settle step private to it; `check-sql.sh` rejects SQL
//! that writes `pr_state` anywhere else.
//!
//! A writer therefore never reads another group's columns and writes them
//! back later. When that happened, a sync that read the row before an
//! engage recorded its answer, and wrote after, put the old engagement
//! watermark back and flagged the answered comments again.
//!
//! One exception: settle voids an answer's decline once it no longer
//! matches the PR ([`Settled::void_addressed`]). That is part of the
//! derivation, computed from the row as it stands in the same
//! transaction, not from a copy read earlier.

use crate::domain::FindingStatus;
use crate::forge::{self, CheckConclusion, GhCheckRun, Mergeable, PrView, ReviewDecision};
use crate::store::{PrRead, RecordedPrState, SeenActivity};
use crate::transition::PrRef;

/// What sync observed on the forge for an open PR: the only `pr_state`
/// input sync writes for one.
///
/// Made only from a forge read ([`Self::from_view`]), which takes the
/// [`PrRead`] begun before that read: an observation cannot be assembled
/// by hand, and names the PR it was read from -- its number included, which
/// is the read's [`PrRef`]'s, so it cannot record one PR's number after
/// checking another's URL. The fields are private for that reason; the
/// store reads them through the accessors. It borrows the read's
/// [`PrSyncPass`](crate::store::PrSyncPass) (`'p`), so it can be recorded
/// only while that pass is live.
#[derive(Debug, PartialEq, Eq)]
pub struct ObservedOpenPr<'p> {
    read: PrRead<'p>,
    mergeable: Mergeable,
    /// The PR's review decision is "changes requested".
    changes_requested: bool,
    /// Human summary of the check rollup (`"2 pass / 1 fail"`).
    checks: Option<String>,
    /// Names of the failing checks, sorted and unique.
    failing_checks: Vec<String>,
    head_ref: String,
    head_sha: String,
    /// Newest feedback from anyone but hunter itself (epoch ms).
    last_activity_at: i64,
}

/// [`ObservedOpenPr`]'s fields as a test sets them: a fixture, only in a
/// `test-support` build ([`ObservedOpenPr::fixture`]).
#[cfg(feature = "test-support")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedFixture {
    pub mergeable: Mergeable,
    pub changes_requested: bool,
    pub checks: Option<String>,
    pub failing_checks: Vec<String>,
    pub head_ref: String,
    pub head_sha: String,
    pub last_activity_at: i64,
}

impl<'p> ObservedOpenPr<'p> {
    /// What the forge read `read` returned for its open PR.
    pub fn from_view(read: PrRead<'p>, pr: &PrView) -> Self {
        let (checks, failing_checks) = checks_summary(&pr.status_check_rollup);
        Self {
            read,
            mergeable: pr.mergeable,
            changes_requested: pr.review_decision == ReviewDecision::ChangesRequested,
            checks,
            failing_checks,
            head_ref: pr.head_ref.clone(),
            head_sha: pr.head_sha.clone(),
            last_activity_at: latest_activity_ms(pr),
        }
    }

    /// An observation with the fields a test chose, still carrying a real
    /// [`PrRead`]: only in a `test-support` build.
    #[cfg(feature = "test-support")]
    pub fn fixture(read: PrRead<'p>, f: ObservedFixture) -> Self {
        Self {
            read,
            mergeable: f.mergeable,
            changes_requested: f.changes_requested,
            checks: f.checks,
            failing_checks: f.failing_checks,
            head_ref: f.head_ref,
            head_sha: f.head_sha,
            last_activity_at: f.last_activity_at,
        }
    }

    /// The failing checks' names, as `pr_state.failing_checks` stores them:
    /// sorted, unique, comma-joined; `None` when none fail.
    pub fn failing_checks_column(&self) -> Option<String> {
        (!self.failing_checks.is_empty()).then(|| self.failing_checks.join(","))
    }

    /// The read this came from: its PR and its `synced_at`.
    pub fn read(&self) -> &PrRead<'p> {
        &self.read
    }

    /// The number of the PR read: its [`PrRef`]'s.
    pub fn pr_number(&self) -> i64 {
        self.read.pr().number()
    }

    pub fn mergeable(&self) -> Mergeable {
        self.mergeable
    }

    pub fn changes_requested(&self) -> bool {
        self.changes_requested
    }

    pub fn checks(&self) -> Option<&str> {
        self.checks.as_deref()
    }

    pub fn head_ref(&self) -> &str {
        &self.head_ref
    }

    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }

    pub fn last_activity_at(&self) -> i64 {
        self.last_activity_at
    }
}

/// Max of createdAt from comments + submittedAt from reviews -> epoch ms.
///
/// Over screened feedback only (`forge::screen_feedback`): a comment from
/// someone who cannot push, and is not a configured bot, never raises
/// `new_comments`, so it cannot start an engage run. Neither does hunter's
/// own ([`Voice::Own`](forge::Voice::Own)): its reply answers the feedback.
pub(crate) fn latest_activity_ms(pr: &PrView) -> i64 {
    let theirs = |v: forge::Voice| v != forge::Voice::Own;
    let comment_stamps = pr
        .comments
        .iter()
        .filter(|c| theirs(c.voice))
        .map(|c| crate::scheduler::iso_ms(&c.created_at));
    let review_stamps = pr
        .reviews
        .iter()
        .filter(|r| theirs(r.voice))
        .map(|r| crate::scheduler::iso_ms(&r.submitted_at));
    comment_stamps.chain(review_stamps).max().unwrap_or(0)
}

/// (human summary, sorted-unique failing check names).
fn checks_summary(rollup: &[GhCheckRun]) -> (Option<String>, Vec<String>) {
    if rollup.is_empty() {
        return (None, Vec::new());
    }
    let named: Vec<(&str, CheckConclusion)> = rollup
        .iter()
        .map(|c| {
            let name = c.name.as_deref().or(c.context.as_deref()).unwrap_or("?");
            let raw = c.conclusion.as_deref().or(c.state.as_deref()).unwrap_or("");
            (name, CheckConclusion::parse(raw))
        })
        .collect();

    let mut failing_set: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut fail_count = 0usize;
    let mut pass_count = 0usize;
    for &(name, concl) in &named {
        if concl.is_failing() {
            failing_set.insert(name);
            fail_count += 1;
        } else if concl.is_passing() {
            pass_count += 1;
        }
    }
    let pending = named.len() - fail_count - pass_count;
    let failing_names: Vec<String> = failing_set.iter().map(|s| (*s).to_owned()).collect();

    let mut parts = vec![format!("{pass_count} pass")];
    if !failing_names.is_empty() {
        parts.push(format!("{fail_count} fail"));
    }
    if pending > 0 {
        parts.push(format!("{pending} pending"));
    }
    (Some(parts.join(" / ")), failing_names)
}

/// What an engage answered: the only `pr_state` input engage writes.
///
/// Fields private, made only by the constructors below, each of which
/// takes its PR as a [`PrRef`] (URL and number from one parse) and its
/// watermark as a [`SeenActivity`] (only from a forge view, or the job that
/// recorded one): an answer cannot name a PR its engage did not read, or
/// raise the watermark past feedback its worker was not shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngageAnswer<'a> {
    /// The PR answered: the finding's when the engage started.
    /// `pr_state` holds one row per finding, not per PR, so without it an
    /// answer to a PR that ended while the worker ran would land on the
    /// row of the finding's next PR, its watermark and decline included.
    pr: &'a PrRef,
    /// The newest feedback the worker was shown. Anything posted after
    /// it -- while the worker ran, or while the chain was suspended -- is
    /// still new. `None` for a chain started before this was recorded, or
    /// for a push whose reply was not posted, which leaves the watermark
    /// where it is. The watermark never goes down: answering can only
    /// re-raise feedback, never hide it.
    seen: Option<SeenActivity>,
    /// What it did about the PR's static problems.
    problems: ProblemsAnswer<'a>,
}

impl<'a> EngageAnswer<'a> {
    /// A published engage that pushed: its feedback answered up to `seen`,
    /// nothing declined.
    pub const fn pushed(pr: &'a PrRef, seen: Option<SeenActivity>) -> Self {
        Self {
            pr,
            seen,
            problems: ProblemsAnswer::Pushed,
        }
    }

    /// A published engage that did not push: its feedback answered up to
    /// `seen`, and the problems recorded in `started_on`, the row it
    /// started from, declined ([`ProblemsAnswer::without_push`]).
    pub fn without_push(
        pr: &'a PrRef,
        seen: Option<SeenActivity>,
        started_on: &'a RecordedPrState,
    ) -> Self {
        Self {
            pr,
            seen,
            problems: ProblemsAnswer::without_push(started_on),
        }
    }

    /// A push that landed with the rest of the engage unpublished (its
    /// reply failed): the PR's head moved, so the push is counted
    /// ([`ProblemsAnswer::Pushed`]), but no feedback is answered and the
    /// watermark stays.
    pub const fn push_only(pr: &'a PrRef) -> Self {
        Self::pushed(pr, None)
    }

    /// An answer with the parts a test chose: only in a `test-support`
    /// build.
    #[cfg(feature = "test-support")]
    pub const fn fixture(
        pr: &'a PrRef,
        seen: Option<SeenActivity>,
        problems: ProblemsAnswer<'a>,
    ) -> Self {
        Self { pr, seen, problems }
    }

    pub const fn pr(&self) -> &'a PrRef {
        self.pr
    }

    pub const fn seen(&self) -> Option<SeenActivity> {
        self.seen
    }

    pub const fn problems(&self) -> ProblemsAnswer<'a> {
        self.problems
    }
}

/// What an engage did about the static problems recorded for its PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemsAnswer<'a> {
    /// It pushed new commits. New code always deserves a fresh look, so
    /// nothing is declined. The problems sync recorded before the push
    /// describe an older commit, not the pushed one: the push is counted
    /// on the row (`push_count`, migration 030), and an observation from a
    /// read begun before it does not count its static problems
    /// ([`Inputs::predates_push`]). Counting them flagged the PR again
    /// before any sync had seen the new head.
    Pushed,
    /// It answered without pushing, declining these problems on this head.
    Declined(Declined<'a>),
    /// It answered without pushing, and the row it started from recorded no
    /// problems, or no head, to decline.
    NoneRecorded,
}

impl<'a> ProblemsAnswer<'a> {
    /// An answer without a push: the problems `ps` records declined, or
    /// [`Self::NoneRecorded`] when it records none to decline.
    pub fn without_push(ps: &'a RecordedPrState) -> Self {
        Declined::as_read(ps).map_or(Self::NoneRecorded, Self::Declined)
    }

    /// The decline it records, if any.
    pub fn declined(&self) -> Option<Declined<'a>> {
        match *self {
            Self::Declined(d) => Some(d),
            Self::Pushed | Self::NoneRecorded => None,
        }
    }

    /// Whether it pushed.
    pub fn pushed(&self) -> bool {
        matches!(self, Self::Pushed)
    }
}

/// A decline: the problems' fingerprint and the head the worker saw them
/// on, both as the engage read them from `pr_state` when it started.
///
/// Made only from that read ([`Self::as_read`]), a [`RecordedPrState`],
/// which only the store makes: a decline can name only problems and a head
/// that a sync recorded, never a string put together elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declined<'a> {
    fingerprint: &'a str,
    head_sha: &'a str,
}

impl<'a> Declined<'a> {
    /// The problems `ps` records, on the head it records them on, as
    /// declined; `None` when it records no problems or no head.
    pub fn as_read(ps: &'a RecordedPrState) -> Option<Self> {
        Some(Self {
            fingerprint: ps.attention_fingerprint.as_deref()?,
            head_sha: ps.head_sha.as_deref()?,
        })
    }

    pub fn fingerprint(&self) -> &'a str {
        self.fingerprint
    }

    pub fn head_sha(&self) -> &'a str {
        self.head_sha
    }
}

/// The static (non-comment) problems sync observed on a PR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Problems {
    pub changes_requested: bool,
    pub conflicting: bool,
    pub checks_failing: bool,
}

/// One `pr_state` row's attention inputs, read inside the transaction that
/// settles it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inputs {
    /// The PR is open. An ended PR needs no engage.
    pub open: bool,
    pub last_activity_at: i64,
    pub problems: Problems,
    pub fingerprint: Option<String>,
    pub head_sha: Option<String>,
    pub engaged_at: i64,
    pub addressed_fingerprint: Option<String>,
    pub addressed_head_sha: Option<String>,
    /// The observation is from a read that began before the engage's last
    /// push: its head, and the static problems on it, are older than what
    /// the PR now holds, whichever commits were pushed in between.
    pub predates_push: bool,
}

/// What settle writes back from [`Inputs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// The recorded decline no longer describes the PR: its fingerprint or
    /// its head moved since. It is cleared, so if the problems it covered
    /// come back, they are flagged again.
    pub void_addressed: bool,
    /// Comma-joined reasons, `None` when the PR needs nothing.
    pub attention: Option<String>,
}

/// The attention a PR's inputs call for.
///
/// * `new_comments`: feedback newer than the engagement watermark.
/// * `changes_requested`, `conflict`, `checks_failing`: the static
///   problems, unless an engage already declined exactly these, on this
///   head. Only a matching fingerprint *and* head suppress them: a push
///   always gets a fresh look, even when the same check is still red.
///   Nor are they counted from an observation whose read began before an
///   engage's last push: they are an older commit's, and no sync has read
///   the pushed one yet.
pub fn settle(i: &Inputs) -> Settled {
    let void_addressed = i.addressed_fingerprint.is_some()
        && (i.fingerprint != i.addressed_fingerprint || i.head_sha != i.addressed_head_sha);
    if !i.open {
        return Settled {
            void_addressed,
            attention: None,
        };
    }
    // Not void and recorded: the decline's fingerprint and head are the
    // PR's current ones.
    let suppressed =
        !void_addressed && i.addressed_fingerprint.is_some() && i.addressed_head_sha.is_some();
    let mut reasons: Vec<&str> = Vec::new();
    if i.last_activity_at > i.engaged_at {
        reasons.push("new_comments");
    }
    if !suppressed && !i.predates_push {
        let p = i.problems;
        if p.changes_requested {
            reasons.push("changes_requested");
        }
        if p.conflicting {
            reasons.push("conflict");
        }
        if p.checks_failing {
            reasons.push("checks_failing");
        }
    }
    Settled {
        void_addressed,
        attention: (!reasons.is_empty()).then(|| reasons.join(",")),
    }
}

/// How settle moved a PR's attention.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttentionChange {
    pub before: Option<String>,
    pub now: Option<String>,
}

impl AttentionChange {
    /// The reasons, when this write raised them or changed them.
    pub fn raised(&self) -> Option<&str> {
        self.now.as_deref().filter(|_| self.now != self.before)
    }
}

/// The outcome of recording a sync's observation of an open PR.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPrRecorded {
    /// Recorded, with how it moved the PR's attention.
    Recorded(AttentionChange),
    /// Nothing written: the finding left `pr_open` after sync listed it.
    /// `now` is where it is (`None`: there is no such finding). Whatever
    /// moved it, such as an engage withdrawing the PR, recorded the PR's
    /// end; writing the older observation would put the row back to OPEN
    /// under a finding that is no longer, and sync revisits only `pr_open`
    /// findings, so nothing would correct it.
    NotPrOpen { now: Option<FindingStatus> },
    /// Nothing written: the finding is `pr_open`, but with another PR than
    /// the one this read was of. The read's PR ended after the read began,
    /// and the finding has shipped a new one since; recording the old PR's
    /// view would put it, and its number, on the new PR's row.
    OtherPr,
}

/// The outcome of recording an engage's answer.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerRecorded {
    /// Recorded, with how it moved the PR's attention.
    Recorded(AttentionChange),
    /// Nothing written: the finding left `pr_open` while the engage ran
    /// (`now`: where it is; `None`: there is no such finding). The PR
    /// answered has ended, and an ended PR needs no answer.
    NotPrOpen { now: Option<FindingStatus> },
    /// Nothing written: the finding is `pr_open` with another PR than the
    /// one answered. The answered PR ended and the finding shipped a new
    /// one while the engage ran; the row is the new PR's.
    OtherPr,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Inputs {
        Inputs {
            open: true,
            head_sha: Some("h1".into()),
            ..Inputs::default()
        }
    }

    fn with_problems(fp: &str) -> Inputs {
        Inputs {
            problems: Problems {
                changes_requested: true,
                conflicting: false,
                checks_failing: true,
            },
            fingerprint: Some(fp.into()),
            ..open()
        }
    }

    const ALL_PROBLEMS: Problems = Problems {
        changes_requested: true,
        conflicting: true,
        checks_failing: true,
    };

    #[test]
    fn feedback_newer_than_the_watermark_is_new_comments_and_older_is_not() {
        let fresh = Inputs {
            last_activity_at: 10,
            engaged_at: 9,
            ..open()
        };
        assert_eq!(settle(&fresh).attention.as_deref(), Some("new_comments"));
        let answered = Inputs {
            engaged_at: 10,
            ..fresh
        };
        assert_eq!(settle(&answered).attention, None);
    }

    #[test]
    fn a_decline_suppresses_exactly_the_problems_and_head_it_answered() {
        let declined = Inputs {
            addressed_fingerprint: Some("fp".into()),
            addressed_head_sha: Some("h1".into()),
            ..with_problems("fp")
        };
        assert_eq!(
            settle(&declined),
            Settled {
                void_addressed: false,
                attention: None
            }
        );
        // Feedback is never suppressed by a decline.
        let commented = Inputs {
            last_activity_at: 5,
            ..declined.clone()
        };
        assert_eq!(
            settle(&commented).attention.as_deref(),
            Some("new_comments")
        );
        // Another check failing is another problem.
        let moved = Inputs {
            fingerprint: Some("fp2".into()),
            ..declined.clone()
        };
        assert_eq!(
            settle(&moved),
            Settled {
                void_addressed: true,
                attention: Some("changes_requested,checks_failing".into())
            }
        );
        // A push gets a fresh look at the same problems.
        let pushed = Inputs {
            head_sha: Some("h2".into()),
            ..declined
        };
        assert_eq!(
            settle(&pushed),
            Settled {
                void_addressed: true,
                attention: Some("changes_requested,checks_failing".into())
            }
        );
    }

    #[test]
    fn problems_gone_void_the_decline_so_their_return_is_flagged() {
        let resolved = Inputs {
            addressed_fingerprint: Some("fp".into()),
            addressed_head_sha: Some("h1".into()),
            ..open()
        };
        assert_eq!(
            settle(&resolved),
            Settled {
                void_addressed: true,
                attention: None
            }
        );
    }

    #[test]
    fn problems_read_before_a_push_wait_for_a_read_after_it() {
        // The observation's read began before the engage's last push.
        let before_push = Inputs {
            predates_push: true,
            ..with_problems("fp")
        };
        assert_eq!(settle(&before_push).attention, None);
        // Feedback is still new: only the older commit's problems wait.
        let commented = Inputs {
            last_activity_at: 5,
            ..before_push
        };
        assert_eq!(
            settle(&commented).attention.as_deref(),
            Some("new_comments")
        );
        // A read begun after the push sees the PR still red: flagged.
        let after_push = with_problems("fp");
        assert_eq!(
            settle(&after_push).attention.as_deref(),
            Some("changes_requested,checks_failing")
        );
    }

    #[test]
    fn an_ended_pr_needs_nothing() {
        let merged = Inputs {
            open: false,
            last_activity_at: 10,
            problems: ALL_PROBLEMS,
            ..with_problems("fp")
        };
        assert_eq!(settle(&merged).attention, None);
    }

    #[test]
    fn reasons_list_in_a_fixed_order() {
        let all = Inputs {
            last_activity_at: 1,
            problems: ALL_PROBLEMS,
            ..with_problems("fp")
        };
        assert_eq!(
            settle(&all).attention.as_deref(),
            Some("new_comments,changes_requested,conflict,checks_failing")
        );
    }

    #[test]
    fn raised_only_when_the_reasons_change_to_something() {
        let same = AttentionChange {
            before: Some("conflict".into()),
            now: Some("conflict".into()),
        };
        assert_eq!(same.raised(), None);
        let changed = AttentionChange {
            before: Some("conflict".into()),
            now: Some("conflict,checks_failing".into()),
        };
        assert_eq!(changed.raised(), Some("conflict,checks_failing"));
        let cleared = AttentionChange {
            before: Some("conflict".into()),
            now: None,
        };
        assert_eq!(cleared.raised(), None);
    }

    #[test]
    fn errored_github_checks_count_as_failing_not_pending() {
        let run = |name: &str, conclusion: &str| GhCheckRun {
            name: Some(name.into()),
            conclusion: Some(conclusion.into()),
            ..GhCheckRun::default()
        };
        let rollup = [
            // A legacy commit status (GitHub's StatusContext) in `ERROR`.
            GhCheckRun {
                context: Some("ci/legacy".into()),
                state: Some("ERROR".into()),
                ..GhCheckRun::default()
            },
            // A check run whose workflow never started.
            run("build", "STARTUP_FAILURE"),
            run("lint", "SUCCESS"),
        ];
        let (summary, failing) = checks_summary(&rollup);
        assert_eq!(failing, ["build", "ci/legacy"]);
        assert_eq!(summary.as_deref(), Some("1 pass / 2 fail"));
    }
}
