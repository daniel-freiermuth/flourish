//! What each scheduler lane is doing right now, for the status page.
//!
//! The job lane does one thing at a time: its current phase is set only
//! through a guard ([`ActivityGuard`]) that puts back what was there before
//! when it drops, so the status never names work that has ended, however
//! that work returned, and a phase begun inside another (a fetch inside a
//! tree's preparation) hands the status back to the outer one.
//!
//! The admin lane runs its duties side by side (`crate::daemon`), so its
//! status is the set of duties running now ([`AdminStatus`]): each guard
//! ([`AdminTaskGuard`]) adds its own entry and removes only that one.
//!
//! The job lane reports through a task-local ([`reporting_job_phases`],
//! [`job_phase`]) rather than through every runner's arguments: the phases
//! are set in helpers deep inside the runners (a fetch, a push), which the
//! admin lane also calls -- its fetch-ahead -- and there they must report
//! nothing. Outside the job lane's task [`job_phase`] is a no-op. A
//! blocking thread does not inherit the task-local, so phases are set
//! from the async code around it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::types::{AdminActivity, AdminTask, JobActivity, JobPhase};

/// One lane's current activity, shared with the status page.
pub struct LaneStatus<T>(Arc<Mutex<Option<T>>>);

impl<T> Default for LaneStatus<T> {
    fn default() -> Self {
        Self(Arc::default())
    }
}

impl<T> Clone for LaneStatus<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T: Copy> LaneStatus<T> {
    /// The activity running now, if any.
    pub fn current(&self) -> Option<T> {
        *self.cell()
    }

    /// Show `activity` until the guard drops, then what was shown before.
    fn show(&self, activity: T) -> ActivityGuard<T> {
        let previous = self.cell().replace(activity);
        ActivityGuard {
            status: self.clone(),
            previous,
        }
    }

    /// Never held across an await, so a poisoned lock only means a panic
    /// between two plain assignments: the value is intact.
    fn cell(&self) -> MutexGuard<'_, Option<T>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An activity in progress; dropped, the lane shows what it showed before.
#[must_use = "the activity shows only while its guard is held"]
pub struct ActivityGuard<T: Copy> {
    status: LaneStatus<T>,
    previous: Option<T>,
}

impl<T: Copy> Drop for ActivityGuard<T> {
    fn drop(&mut self) {
        *self.status.cell() = self.previous;
    }
}

/// The admin lane's duties running now (`crate::daemon`), each from its
/// [`Self::begin`] until its guard drops.
#[derive(Clone, Default)]
pub struct AdminStatus(Arc<AdminRunning>);

#[derive(Default)]
struct AdminRunning {
    /// Each live guard's entry, by the id only that guard holds.
    running: Mutex<Vec<(u64, AdminActivity)>>,
    next: AtomicU64,
}

impl AdminStatus {
    /// Record `task` as running from now until the guard is dropped.
    pub fn begin(&self, task: AdminTask) -> AdminTaskGuard {
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let activity = AdminActivity {
            task,
            started_at: crate::util::now_ms(),
        };
        self.entries().push((id, activity));
        AdminTaskGuard {
            status: self.clone(),
            id,
        }
    }

    /// The duties running now, by task.
    pub fn current(&self) -> Vec<AdminActivity> {
        let mut now: Vec<AdminActivity> = self.entries().iter().map(|&(_, a)| a).collect();
        now.sort_by_key(|a| (a.task, a.started_at));
        now
    }

    /// Never held across an await, so a poisoned lock only means a panic
    /// between two plain pushes or removals: the list is intact.
    fn entries(&self) -> MutexGuard<'_, Vec<(u64, AdminActivity)>> {
        self.0
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A duty in progress; dropped, it leaves the admin lane's status, and
/// nothing else does.
#[must_use = "the duty shows only while its guard is held"]
pub struct AdminTaskGuard {
    status: AdminStatus,
    id: u64,
}

impl Drop for AdminTaskGuard {
    fn drop(&mut self) {
        self.status.entries().retain(|&(id, _)| id != self.id);
    }
}

/// The job lane's current phase: what a cycle does outside its worker.
pub type JobStatus = LaneStatus<JobActivity>;

impl JobStatus {
    /// Record `phase` as running from now until the guard is dropped.
    /// Crate-private: the job lane's phases are set inside the cycle
    /// `run_cycle` runs under [`reporting_job_phases`] ([`job_phase`]), so
    /// no handler or admin duty can show a job phase.
    pub(crate) fn begin(&self, phase: JobPhase) -> ActivityGuard<JobActivity> {
        self.show(JobActivity {
            phase,
            started_at: crate::util::now_ms(),
        })
    }
}

tokio::task_local! {
    static JOB_STATUS: JobStatus;
}

/// Run `cycle` with its phases ([`job_phase`]) reported to `status`. Only
/// `run_cycle` does, with the job lane's own status.
pub(crate) async fn reporting_job_phases<F: Future>(status: JobStatus, cycle: F) -> F::Output {
    JOB_STATUS.scope(status, cycle).await
}

/// Report `phase` for the job lane until the guard drops; `None`, and
/// nothing reported, outside the job lane's cycle.
pub fn job_phase(phase: JobPhase) -> Option<ActivityGuard<JobActivity>> {
    JOB_STATUS.try_with(|status| status.begin(phase)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(status: &JobStatus) -> Option<JobPhase> {
        status.current().map(|a| a.phase)
    }

    /// A phase begun inside another hands the status back to the outer one
    /// when it ends, and the outer one's end clears it.
    #[test]
    fn an_inner_phase_hands_the_status_back_to_the_outer_one() {
        let status = JobStatus::default();
        let outer = status.begin(JobPhase::PreparingTree);
        let inner = status.begin(JobPhase::Fetching);
        assert_eq!(phase(&status), Some(JobPhase::Fetching));
        drop(inner);
        assert_eq!(phase(&status), Some(JobPhase::PreparingTree));
        drop(outer);
        assert_eq!(phase(&status), None);
    }

    /// Only the job lane's cycle reports: the same helper called from any
    /// other task (the admin lane's fetch-ahead) shows nothing.
    #[tokio::test]
    async fn only_the_job_lanes_cycle_reports_its_phases() {
        let status = JobStatus::default();
        assert!(job_phase(JobPhase::Fetching).is_none());
        let seen = reporting_job_phases(status.clone(), async {
            let _guard = job_phase(JobPhase::Fetching);
            phase(&status)
        })
        .await;
        assert_eq!(seen, Some(JobPhase::Fetching));
        assert_eq!(phase(&status), None);
    }
}
