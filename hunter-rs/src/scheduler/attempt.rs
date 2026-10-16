//! What a runner is handed: the subject it works on, and for a resume the
//! plan it continues, as one value whose parts cannot disagree.
//!
//! A plan names its job's kind and subject itself, and the job row a
//! resume creates follows the plan (`JobStart::Resume`). Handed to a
//! runner beside a separately chosen subject, the plan of one finding
//! would run another finding's work under the first one's row, and a plan
//! of one kind would run another kind's logic under the first kind's row.
//! So a runner takes an [`Attempt`] of its own kind: a cold one carries
//! only its subject, and a resumed one carries a [`Resumed`], whose only
//! constructor refuses a plan of another kind and reads the subject from
//! the plan's own ids.

use std::marker::PhantomData;

use super::ResumePlan;
use crate::domain::{FindingJobKind, JobKind, RepoJobKind};
use crate::store::Store;
use crate::types::{Finding, Repo};

mod sealed {
    pub trait Sealed {}
}

/// A job kind the compiler knows: one marker type per kind, so a runner's
/// signature says which kind's plans it runs. Sealed: each kind's subject
/// is the daemon's to fix, and a marker made elsewhere could pair a kind
/// with a subject its plans do not name.
pub trait Kind: sealed::Sealed + Send + Sync + 'static {
    const KIND: JobKind;
    type Subject: Subject;
}

/// What a kind works on, read from the plan's own ids.
pub trait Subject: Sized + Send + Sync {
    fn of_plan(
        store: &Store,
        plan: &ResumePlan,
    ) -> impl Future<Output = anyhow::Result<Self>> + Send;
}

impl Subject for Repo {
    async fn of_plan(store: &Store, plan: &ResumePlan) -> anyhow::Result<Self> {
        let repo_id = plan.repo_id;
        store
            .get_repo_by_id(repo_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("repo {repo_id} not found"))
    }
}

impl Subject for Finding {
    async fn of_plan(store: &Store, plan: &ResumePlan) -> anyhow::Result<Self> {
        // A finding-kind job always recorded its target, so a row without
        // one is corrupt rather than merely odd.
        let finding_id = plan
            .finding_id
            .ok_or_else(|| anyhow::anyhow!("resume of a {} job with no finding", plan.kind))?;
        store
            .get_finding(finding_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("finding {finding_id} not found"))
    }
}

/// The marker type of each job kind ([`Kind`]).
pub mod kind {
    use super::{Finding, FindingJobKind, JobKind, Kind, Repo, RepoJobKind, sealed::Sealed};

    macro_rules! kinds {
        ($($name:ident: $kind:expr => $subject:ty;)*) => {$(
            pub struct $name;
            impl Sealed for $name {}
            impl Kind for $name {
                const KIND: JobKind = $kind;
                type Subject = $subject;
            }
        )*};
    }

    kinds! {
        Hunt: JobKind::Repo(RepoJobKind::Hunt) => Repo;
        TestGap: JobKind::Repo(RepoJobKind::TestGap) => Repo;
        DepUpdate: JobKind::Repo(RepoJobKind::DepUpdate) => Repo;
        Refactor: JobKind::Repo(RepoJobKind::Refactor) => Repo;
        Modernization: JobKind::Repo(RepoJobKind::Modernization) => Repo;
        Standards: JobKind::Repo(RepoJobKind::Standards) => Repo;
        Recheck: JobKind::Finding(FindingJobKind::Recheck) => Finding;
        Fix: JobKind::Finding(FindingJobKind::Fix) => Finding;
        Engage: JobKind::Finding(FindingJobKind::Engage) => Finding;
        Harvest: JobKind::Finding(FindingJobKind::Harvest) => Finding;
    }
}

/// A plan of kind `K`, with the subject it names. Borrows the plan: the
/// one thing it adds is the subject, read once where the plan enters a
/// runner.
pub struct Resumed<'p, K: Kind> {
    plan: &'p ResumePlan,
    subject: K::Subject,
    _kind: PhantomData<K>,
}

impl<'p, K: Kind> Resumed<'p, K> {
    /// The only constructor: refuses a plan of another kind, and reads the
    /// subject from the plan. A plan comes from a job row, so this is
    /// where its kind meets the runner's; past it, a mismatch of either
    /// cannot be written down.
    pub async fn load(store: &Store, plan: &'p ResumePlan) -> anyhow::Result<Self> {
        if plan.kind != K::KIND {
            anyhow::bail!("{} plan handed to the {} runner", plan.kind, K::KIND);
        }
        let subject = K::Subject::of_plan(store, plan).await?;
        Ok(Self {
            plan,
            subject,
            _kind: PhantomData,
        })
    }
}

/// One attempt at kind `K`'s work: a cold start on a subject, or a resume
/// whose subject is its plan's.
pub enum Attempt<'a, K: Kind> {
    Cold(&'a K::Subject),
    Resume(&'a Resumed<'a, K>),
}

// By hand: a derive would require `K: Clone`, and the markers are never
// values.
impl<K: Kind> Clone for Attempt<'_, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: Kind> Copy for Attempt<'_, K> {}

impl<'a, K: Kind> Attempt<'a, K> {
    /// What this attempt works on.
    pub fn subject(self) -> &'a K::Subject {
        match self {
            Self::Cold(subject) => subject,
            Self::Resume(resumed) => &resumed.subject,
        }
    }

    /// The plan a resume continues; `None` for a cold attempt.
    pub fn plan(self) -> Option<&'a ResumePlan> {
        match self {
            Self::Cold(_) => None,
            Self::Resume(resumed) => Some(resumed.plan),
        }
    }
}
