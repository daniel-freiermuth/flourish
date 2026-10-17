//! Finding status changes outside a fix claim (WORK-MODEL.md §5).
//!
//! Every such change is one [`Transition`]. The variant names the edge, and
//! with it the statuses the change starts from ([`Sources`]). For an
//! operator's verdict that is the status `/api/verdict` read before
//! checking the verdict against it; for a hold's release, the status read
//! with the hold in the same transaction.
//! The store writes the change only while the finding is still at one of
//! them ([`Store::transition`], or inside a writer's own transaction). A
//! writer that finds it has moved on gets [`Moved::Conflict`] back and
//! decides what that means: no writer can overwrite whatever moved the
//! finding in between.
//!
//! `fixing` is in no edge, and cannot be: every edge's statuses are
//! [`Unclaimed`], which has no `fixing`. A fix claim is taken and released
//! only by its holder ([`crate::recovery::FixClaim`]), or by recovery for a
//! claim whose holder is gone. Those moves are the only status writes
//! outside this module's edges.
//!
//! [`Store::transition`]: crate::store::Store::transition

use crate::domain::{ClosureClass, FindingStatus};
use crate::forge::Forge;
use crate::store::{FindingAnalysisUpdate, FindingInsert, ScanEdge, VerdictAnchor};

/// A finding status no fix claim holds: every [`FindingStatus`] but
/// `fixing`, which only a [`crate::recovery::FixClaim`]'s holder moves a
/// finding into or out of. A [`Transition`] starts and ends at one of
/// these, so no transition can touch a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unclaimed {
    New,
    Rechecking,
    Queued,
    PrOpen,
    Merged,
    Closed,
    Superseded,
    Rejected,
    Wontfix,
    Note,
}

impl From<Unclaimed> for FindingStatus {
    fn from(s: Unclaimed) -> Self {
        match s {
            Unclaimed::New => Self::New,
            Unclaimed::Rechecking => Self::Rechecking,
            Unclaimed::Queued => Self::Queued,
            Unclaimed::PrOpen => Self::PrOpen,
            Unclaimed::Merged => Self::Merged,
            Unclaimed::Closed => Self::Closed,
            Unclaimed::Superseded => Self::Superseded,
            Unclaimed::Rejected => Self::Rejected,
            Unclaimed::Wontfix => Self::Wontfix,
            Unclaimed::Note => Self::Note,
        }
    }
}

/// `fixing`, which is held by a fix claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claimed;

impl TryFrom<FindingStatus> for Unclaimed {
    type Error = Claimed;

    fn try_from(s: FindingStatus) -> Result<Self, Claimed> {
        use FindingStatus as S;
        Ok(match s {
            S::New => Self::New,
            S::Rechecking => Self::Rechecking,
            S::Queued => Self::Queued,
            S::Fixing => return Err(Claimed),
            S::PrOpen => Self::PrOpen,
            S::Merged => Self::Merged,
            S::Closed => Self::Closed,
            S::Superseded => Self::Superseded,
            S::Rejected => Self::Rejected,
            S::Wontfix => Self::Wontfix,
            S::Note => Self::Note,
        })
    }
}

impl std::fmt::Display for Unclaimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        FindingStatus::from(*self).fmt(f)
    }
}

impl From<ClosureClass> for Unclaimed {
    /// The status a closed PR's classification gives its finding. Only
    /// `wrong` and `unwanted` suppress, because only they say something
    /// about the finding: its premise failed, or the maintainers do not want
    /// that kind of change. Work that landed another way, or whose code is
    /// gone, says the finding was right. `abandoned` says nothing at all, so
    /// the finding goes back to triage.
    fn from(class: ClosureClass) -> Self {
        match class {
            ClosureClass::Superseded | ClosureClass::Duplicate | ClosureClass::Obsolete => {
                Self::Superseded
            }
            ClosureClass::Wrong => Self::Rejected,
            ClosureClass::Unwanted => Self::Wontfix,
            ClosureClass::Abandoned => Self::New,
        }
    }
}

/// The status a declined fix gives its finding: its classification's, as
/// a closed PR's (`Unclaimed: From<ClosureClass>`), and triage (`new`) when the
/// worker gave none, as for an `abandoned` closure. The only statuses a
/// fix claim can conclude to ([`crate::recovery::FixClaim::conclude`]).
pub fn decline_status(class: Option<ClosureClass>) -> Unclaimed {
    class.map_or(Unclaimed::New, Unclaimed::from)
}

/// The statuses a [`Transition`] applies from: one, or either of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sources {
    One(Unclaimed),
    Either(Unclaimed, Unclaimed),
}

impl Sources {
    /// Both bounds of the guard `status IN (?, ?)`: a single source twice.
    pub(crate) fn pair(self) -> (FindingStatus, FindingStatus) {
        match self {
            Self::One(s) => (s.into(), s.into()),
            Self::Either(a, b) => (a.into(), b.into()),
        }
    }
}

impl std::fmt::Display for Sources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::One(s) => s.fmt(f),
            Self::Either(a, b) => write!(f, "{a}|{b}"),
        }
    }
}

/// The verdict an operator's release of a hold writes (`/api/hold/release`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseVerdict {
    Rejected,
    Wontfix,
    /// A closed PR classified by hand.
    Classified(ClosureClass),
}

/// One finding status change and what it writes besides the status.
#[derive(Debug, Clone, Copy)]
pub enum Transition<'a> {
    /// The operator asks for a recheck (`/api/recheck`).
    RecheckRequested,
    /// The operator takes a finding out of the fix queue (`/api/unqueue`).
    Unqueued,
    /// The operator's verdict (`/api/verdict`).
    Verdict(OperatorVerdict<'a>),
    /// The operator released a hold with a verdict. `from` is the status
    /// read with the hold, in the transaction that writes the verdict.
    HoldReleased {
        from: Unclaimed,
        verdict: ReleaseVerdict,
        reason: &'a str,
    },
    /// A recheck confirmed the finding; it goes back to triage, with the
    /// recheck's revised analysis.
    RecheckConfirmed { analysis: &'a FindingAnalysisUpdate },
    /// A recheck found the code moved on, taking the bug with it.
    RecheckStale { reason: &'a str },
    /// A recheck found the finding wrong, anchored to what it judged.
    RecheckInvalid {
        reason: &'a str,
        anchor: Option<&'a VerdictAnchor>,
    },
    /// A scan filed a suppressed finding again and its verdict had lapsed
    /// ([`crate::suppression::reopen_if_changed`]): back to triage with the
    /// scan's analysis, the verdict and its anchor gone.
    Reopened { scan: &'a FindingInsert },
    /// A Renovate scan proposes again a `dep_update` an earlier scan retired
    /// as unproposed: it never landed, so it is triage's again. Made only
    /// inside the store's dependency-scan writes ([`ScanEdge`]).
    Reproposed(ScanEdge),
    /// A Renovate scan no longer proposes a `new` `dep_update`. Made only
    /// inside the admin lane's Renovate writes ([`ScanEdge`]).
    Unproposed { reason: &'a str, scan: ScanEdge },
    /// A queued `dep_update` gives its place in the queue to the scan's
    /// finding for the same move. Made only inside the admin lane's
    /// Renovate writes ([`ScanEdge`]).
    HandedOver { reason: &'a str, scan: ScanEdge },
    /// The scan's finding that takes a handed-over place in the queue, with
    /// the stronger of its own budget override and `from_id`'s. From
    /// `queued` as well: several queued findings can hand over to one group.
    /// Made only beside [`Self::HandedOver`].
    TakesOver { from_id: i64, scan: ScanEdge },
    /// A `pr_open` row without a PR URL (a fix that failed before its PR,
    /// from before fix claims): retried.
    PrMissing,
    /// A closed PR's harvest classified why it closed.
    Classified {
        class: ClosureClass,
        reason: &'a str,
        anchor: Option<&'a VerdictAnchor>,
    },
}

/// The PR a write about a PR is about: its URL, as the writer read it from
/// the finding, and its number, parsed from that URL.
///
/// Made only by [`Self::parse`], so the number and the URL always name the
/// same PR. With both taken separately -- the URL from the finding, the
/// number from `pr_state` -- a write could check one PR and record the
/// other: the store checks the URL against the finding and records the
/// number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    url: String,
    number: i64,
}

impl PrRef {
    /// The PR at `url`, its number parsed by `forge`; `None` when `url` is
    /// not a PR URL of `forge`'s.
    pub fn parse(forge: &dyn Forge, url: &str) -> Option<Self> {
        let (_slug, number) = forge.parse_pr_url(url)?;
        Some(Self {
            url: url.to_owned(),
            number,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub const fn number(&self) -> i64 {
        self.number
    }
}

/// A finding's PR ended: `pr_open` → `merged` or `closed`, with `pr_state`
/// recording the end in the same write ([`Store::end_pr`]).
///
/// Not a [`Transition`]: a transition is guarded by status alone, and
/// `pr_open` cannot tell the PR its writer read from the one the finding
/// went on to ship. `pr_state` holds one row per finding, so an end of the
/// old PR landing after that would close the finding under its open new
/// PR and put the old number on the new PR's row. An end therefore names
/// its PR ([`PrRef`]) and lands only while that is still the finding's
/// ([`PrEnded::OtherPr`] otherwise), as an observation and an answer do
/// (`crate::attention`).
///
/// [`Store::end_pr`]: crate::store::Store::end_pr
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrEnding<'a> {
    /// The PR sync saw the PR merged.
    Merged,
    /// Closed without merging: seen by the PR sync, or withdrawn by
    /// engage. The harvest classifies it.
    Closed { reason: &'a str },
}

/// The verdict reason the PR sync records for a PR it saw closed without
/// merging: a closure nobody has classified yet. An engage that withdrew
/// the same PR takes such a closure over ([`Store::adopt_closure`]).
///
/// [`Store::adopt_closure`]: crate::store::Store::adopt_closure
pub const PR_CLOSED_AWAITING_HARVEST: &str = "PR closed without merge; awaiting harvest";

impl<'a> PrEnding<'a> {
    /// The status an end applies from: only a `pr_open` finding has a PR
    /// to end.
    pub const fn from(&self) -> Unclaimed {
        Unclaimed::PrOpen
    }

    /// The status the end writes.
    pub const fn to(&self) -> Unclaimed {
        match self {
            Self::Merged => Unclaimed::Merged,
            Self::Closed { .. } => Unclaimed::Closed,
        }
    }

    pub(crate) const fn record(&self) -> Record<'a> {
        match *self {
            Self::Merged => Record::StatusOnly,
            Self::Closed { reason } => Record::Verdict {
                reason: Some(reason),
                anchor: None,
            },
        }
    }

    /// How `pr_state` records the end.
    pub(crate) const fn end(&self) -> PrEnd {
        match self {
            Self::Merged => PrEnd::Merged,
            Self::Closed { .. } => PrEnd::Closed,
        }
    }
}

/// The result of a [`PrEnding`]. `#[must_use]`, as [`Moved`]: whatever a
/// caller records about the end it records only when the end was made.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrEnded {
    Applied,
    /// The finding is not `pr_open`: `now` is where it is (`None`: there
    /// is no such finding). Whatever moved it decided it, and recorded the
    /// PR's end if that is what moved it.
    Conflict {
        now: Option<FindingStatus>,
    },
    /// The finding is `pr_open`, but with another PR than the one ended:
    /// that PR ended, and the finding shipped a new one, after the writer
    /// read it. Nothing is written; the row is the new PR's.
    OtherPr,
}

/// How a PR ended, as `pr_state.state` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrEnd {
    Merged,
    Closed,
}

impl PrEnd {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "MERGED",
            Self::Closed => "CLOSED",
        }
    }
}

/// What a transition writes besides the status.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Record<'a> {
    /// Nothing: the verdict reason and anchor stay as they were.
    StatusOnly,
    /// A recheck's revised analysis: each field given replaces the
    /// finding's, each absent one leaves it.
    Analysis(&'a FindingAnalysisUpdate),
    /// A scan's analysis, replacing the finding's; the verdict reason and
    /// anchor are removed.
    Rescan(&'a FindingInsert),
    /// A verdict: its reason (`None` removes an earlier verdict's), and its
    /// anchor replacing any earlier one (`None` removes it). Neither
    /// survives the verdict: both described an earlier one, not this one.
    Verdict {
        reason: Option<&'a str>,
        anchor: Option<&'a VerdictAnchor>,
    },
    /// The stronger budget override of the finding's own and `from_id`'s:
    /// `exempt` over `once` over none. `once` is cleared after the next
    /// attempt, `exempt` only by hand, so a `once` must not displace an
    /// `exempt`.
    InheritOverride { from_id: i64 },
}

impl<'a> Transition<'a> {
    /// The statuses the change applies from.
    pub fn from(&self) -> Sources {
        use Sources::{Either, One};
        use Unclaimed as S;
        match self {
            Self::RecheckRequested | Self::Unproposed { .. } => One(S::New),
            Self::Unqueued | Self::HandedOver { .. } => One(S::Queued),
            Self::Verdict(v) => One(v.from),
            Self::HoldReleased { from, .. } => One(*from),
            Self::RecheckConfirmed { .. }
            | Self::RecheckStale { .. }
            | Self::RecheckInvalid { .. } => One(S::Rechecking),
            Self::Reopened { .. } => Either(S::Rejected, S::Wontfix),
            Self::Reproposed(_) => One(S::Superseded),
            Self::TakesOver { .. } => Either(S::New, S::Queued),
            Self::PrMissing => One(S::PrOpen),
            Self::Classified { .. } => One(S::Closed),
        }
    }

    /// The status the change writes.
    pub fn to(&self) -> Unclaimed {
        use Unclaimed as S;
        match self {
            Self::RecheckRequested => S::Rechecking,
            Self::Unqueued
            | Self::RecheckConfirmed { .. }
            | Self::Reopened { .. }
            | Self::Reproposed(_) => S::New,
            Self::Verdict(v) => v.to,
            Self::HoldReleased { verdict, .. } => match verdict {
                ReleaseVerdict::Rejected => S::Rejected,
                ReleaseVerdict::Wontfix => S::Wontfix,
                ReleaseVerdict::Classified(class) => (*class).into(),
            },
            Self::RecheckStale { .. } | Self::Unproposed { .. } | Self::HandedOver { .. } => {
                S::Superseded
            }
            Self::RecheckInvalid { .. } => S::Rejected,
            Self::PrMissing | Self::TakesOver { .. } => S::Queued,
            Self::Classified { class, .. } => (*class).into(),
        }
    }

    pub(crate) fn record(&self) -> Record<'a> {
        match *self {
            Self::RecheckRequested | Self::Unqueued | Self::PrMissing => Record::StatusOnly,
            Self::RecheckConfirmed { analysis } => Record::Analysis(analysis),
            Self::Reopened { scan } => Record::Rescan(scan),
            Self::TakesOver { from_id, .. } => Record::InheritOverride { from_id },
            // An operator's verdict has no tree to anchor to, and one given
            // without a reason clears the reason an earlier verdict left.
            Self::Verdict(v) => Record::Verdict {
                reason: v.reason,
                anchor: None,
            },
            // The retirement's reason no longer applies.
            Self::Reproposed(_) => Record::Verdict {
                reason: None,
                anchor: None,
            },
            Self::HoldReleased { reason, .. }
            | Self::RecheckStale { reason }
            | Self::Unproposed { reason, .. }
            | Self::HandedOver { reason, .. } => Record::Verdict {
                reason: Some(reason),
                anchor: None,
            },
            Self::RecheckInvalid { reason, anchor } | Self::Classified { reason, anchor, .. } => {
                Record::Verdict {
                    reason: Some(reason),
                    anchor,
                }
            }
        }
    }
}

/// An operator's verdict on a finding that asks for one. Only
/// [`OperatorVerdict::new`] makes one, so every verdict starts at a status
/// that awaits it ([`FindingStatus::awaits_verdict`]), ends at a verdict
/// status ([`FindingStatus::is_verdict`]) other than the one it starts at,
/// and carries the reason its status requires. Neither is `fixing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorVerdict<'a> {
    from: Unclaimed,
    to: Unclaimed,
    reason: Option<&'a str>,
}

/// Why [`OperatorVerdict::new`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictRefused {
    /// `to` is not a status a verdict sets.
    NotAVerdict,
    /// `to` needs a reason and none was given.
    ReasonRequired,
    /// The finding is not at a status that asks for a verdict, or the
    /// verdict would not change it.
    NotAwaiting,
}

impl<'a> OperatorVerdict<'a> {
    /// `from` is the finding's status as the caller read it before
    /// checking the verdict against it; the verdict applies only while the
    /// finding is still there.
    pub fn new(
        from: FindingStatus,
        to: FindingStatus,
        reason: Option<&'a str>,
    ) -> Result<Self, VerdictRefused> {
        if !to.is_verdict() {
            return Err(VerdictRefused::NotAVerdict);
        }
        if to.reason_required() && reason.is_none() {
            return Err(VerdictRefused::ReasonRequired);
        }
        if !from.awaits_verdict() || from == to {
            return Err(VerdictRefused::NotAwaiting);
        }
        // Neither check passes `fixing`; the conversion makes that a type.
        let (Ok(from), Ok(to)) = (Unclaimed::try_from(from), Unclaimed::try_from(to)) else {
            return Err(VerdictRefused::NotAwaiting);
        };
        Ok(Self { from, to, reason })
    }
}

/// The result of a [`Transition`]. `#[must_use]`: a conflict means the
/// change was not made, which the caller has to account for.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moved {
    Applied,
    /// The finding was at none of the transition's sources: `now` is
    /// where it is (`None`: there is no such finding).
    Conflict {
        now: Option<FindingStatus>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use FindingStatus as S;

    #[test]
    fn a_suppressing_verdict_needs_its_reason() {
        assert_eq!(
            OperatorVerdict::new(S::New, S::Rejected, None),
            Err(VerdictRefused::ReasonRequired)
        );
        assert!(OperatorVerdict::new(S::New, S::Note, None).is_ok());
    }

    /// The conversions are each other's inverse, and `fixing` is the one
    /// status without an [`Unclaimed`] counterpart.
    #[test]
    fn every_status_but_fixing_converts_both_ways() {
        for s in S::ALL {
            match Unclaimed::try_from(s) {
                Ok(u) => assert_eq!(FindingStatus::from(u), s),
                Err(Claimed) => assert_eq!(s, S::Fixing),
            }
        }
    }
}
