//! Backend protocol — port of hunter/backend.py (BACKEND-CONTRACT.md §1).
//!
//! A backend answers: (1) may background work spend now, and up to how much
//! (`decide`); (2) run this job (`run`); (3) what's your status
//! (`keep_fresh`/`status_html`). All three are implemented by
//! `OmpScavengeBackend`.
//!
//! Decisions cross this boundary as data (`Outlook`); diagnostics cross as
//! presentation (an HTML fragment the backend fully owns). The typed
//! `WindowPanel` model behind that fragment stays private to the
//! `omp_scavenge` impl: the set of window dimensions a backend reports is
//! its own vocabulary, so typing it here would put one impl's shape into the
//! shared contract. The resulting coupling to the UI's CSS class names is
//! paid for by specifying the markup byte-exactly instead
//! (BACKEND-CONTRACT.md §2.3).

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;

/// The two budget/model classes the scheduler collapses all job kinds into
/// at the backend boundary (`backend.JobClass`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobClass {
    Hunt,
    Fix,
}

impl JobClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hunt => "hunt",
            Self::Fix => "fix",
        }
    }
}

/// Granted/Denied verdict (`backend.Granted` / `backend.Denied`).
///
/// - `Granted.cap_tokens`: the backend's headroom for the job, and the
///   only token bound it gets; None = no token bound at all.
/// - `Denied.retry_at`: epoch ms the denial is expected to resolve; None = no
///   informed estimate (callers use a generic 30-min backoff).
/// - reason strings are prose for notes/events/UI — never machine-matched
///   (tests substring-match only), but formats are kept byte-identical to
///   Python (`{:.2}` half-to-even rounding).
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Granted {
        cap_tokens: Option<i64>,
        reason: String,
    },
    Denied {
        reason: String,
        retry_at: Option<f64>,
    },
}

impl Verdict {
    pub fn is_granted(&self) -> bool {
        matches!(self, Self::Granted { .. })
    }
}

/// Paired verdicts (`backend.Outlook`).
///
/// INVARIANT: prioritized is at least as permissive as normal — if normal
/// is Granted, prioritized is Granted. The scheduler indexes
/// `outlook.prioritized if override else outlook.normal` and never tells
/// the backend which it wanted. `omp_scavenge` guarantees the invariant by
/// construction.
#[derive(Debug, Clone)]
pub struct Outlook {
    pub normal: Verdict,
    pub prioritized: Verdict,
}

/// What a backend may know about hunter's own job spend and observation
/// history (`backend.SpendLedger`). Implemented by Store on the sqlx pool
/// (Python's `ThreadLocalLedger` dissolves — the pool is already
/// thread-safe). Exact SQL: BACKEND-CONTRACT.md §1.6.
#[async_trait]
pub trait SpendLedger: Send + Sync {
    /// `SUM(COALESCE(estimated_tokens, cap_tokens, 0))` over running jobs.
    async fn running_estimate(&self) -> sqlx::Result<i64>;
    /// `SUM(tokens_new)` of non-running jobs finished strictly after `ts_ms`.
    async fn finished_since(&self, ts_ms: i64) -> sqlx::Result<i64>;
    /// Same, half-open (`start_ms`, `end_ms`].
    async fn finished_between(&self, start_ms: i64, end_ms: i64) -> sqlx::Result<i64>;
    /// INSERT INTO `window_log` (`observed_at` = now, `source_age_s` = `trunc(age_s)`).
    async fn log_window_observation(
        &self,
        limit_id: &str,
        used_fraction: Option<f64>,
        status: Option<&str>,
        resets_at: Option<i64>,
        age_s: f64,
    ) -> sqlx::Result<()>;
    /// Newest (`observed_at`, `used_fraction`) for `limit_id` within
    /// `resets_at` ± 5000 ms; None when absent OR `used_fraction` is NULL.
    async fn last_window_observation(
        &self,
        limit_id: &str,
        resets_at: i64,
    ) -> sqlx::Result<Option<(i64, f64)>>;
    /// INSERT INTO `calibration_samples` (`observed_at` = now).
    async fn record_calibration_sample(
        &self,
        limit_id: &str,
        window_resets_at: Option<i64>,
        used_fraction_delta: f64,
        hunter_tokens: i64,
    ) -> sqlx::Result<()>;
    /// Max tokens hunter ever spent inside one completed window cycle
    /// (NOT p75; `SpendLedger.estimate_capacity` / `Store.estimate_capacity`;
    /// the Python `_min_delta` param is dead and dropped here). Some always
    /// contains a strictly positive capacity; None for unknown `limit_ids`
    /// or when no cycle had positive spend.
    async fn estimate_capacity(&self, limit_id: &str) -> sqlx::Result<Option<f64>>;
}

/// Subprocess seam for `keep_fresh`'s `omp usage` probes — injectable so the
/// 9 refresh tests control rc without spawning (mirrors Python tests
/// patching `hunter.util.run_cmd`). `run` mirrors `util::run_cmd`: merged
/// stdout+stderr, (124, msg) on timeout, (127, msg) on spawn error,
/// never fails.
pub trait Prober: Send + Sync {
    fn run(&self, argv: &[&str], timeout_s: u64) -> (i32, String);
}

/// Real prober: delegates to `util::run_cmd`.
pub struct CmdProber;

impl Prober for CmdProber {
    fn run(&self, argv: &[&str], timeout_s: u64) -> (i32, String) {
        crate::util::run_cmd(argv, timeout_s)
    }
}

/// The backend facade — harness, accounting and budget policy behind one
/// object, so the core never sees a provider's vocabulary.
///
/// Errors: Python lets sqlite/IO exceptions propagate to the caller's
/// catch-all (HTTP 500 / cycle error summary); Rust surfaces them as Err
/// with identical handling at the call sites.
#[async_trait]
pub trait Backend: Send + Sync {
    /// May background work spend now? `anticipated_tokens` = caller's
    /// pre-reservation for the job under decision.
    async fn decide(&self, anticipated_tokens: i64) -> anyhow::Result<Outlook>;
    /// Refresh stale accounting, log observations, record calibration.
    /// Returns true iff a probe was performed. Called by the daemon's
    /// prober tick.
    async fn keep_fresh(&self) -> anyhow::Result<bool>;
    /// Status HTML fragment for /`api/summary.backend_status_html`
    /// (innerHTML'd by the UI every 5 s; byte-parity spec in
    /// BACKEND-CONTRACT.md §2.3).
    async fn status_html(&self) -> anyhow::Result<String>;
    /// The hosts a worker needs to reach, each `host` or `host:port` (443
    /// when it names none), for the scheduler's reachability gate
    /// ([`crate::reach::first_unreachable`]). Empty = nothing to probe.
    fn worker_hosts(&self) -> Vec<String>;
    /// Execute a worker job in its chain's workspace: the worker runs in
    /// `ws.tree` and its transcript goes to `ws.session`. `cap_tokens` is
    /// the token bound the ramp
    /// granted, `None` for a job that has none; `job_class` drives model
    /// selection. `resume_from` is the session file of an earlier attempt
    /// to continue instead of starting cold — `None` is a cold run, which
    /// is what every job was before resume existed.
    async fn run(
        &self,
        ws: &crate::workspace::Workspace,
        prompt: &str,
        cap_tokens: Option<i64>,
        max_wall_s: i64,
        job_class: JobClass,
        resume_from: Option<&std::path::Path>,
    ) -> anyhow::Result<crate::types::RunResult>;
}
/// Runtime budget-mode switch shared by the scheduler and HTTP controls.
///
/// Overdrive promotes the backend's prioritized verdict to the normal path
/// for every job. The provider still owns the prioritized verdict, including
/// its hard-stop denial and remaining-headroom cap.
pub struct OverdriveBackend {
    inner: Arc<dyn Backend>,
    enabled: Arc<AtomicBool>,
}

impl OverdriveBackend {
    pub fn new(inner: Arc<dyn Backend>, enabled: Arc<AtomicBool>) -> Self {
        Self { inner, enabled }
    }
}

#[async_trait]
impl Backend for OverdriveBackend {
    async fn decide(&self, anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let outlook = self.inner.decide(anticipated_tokens).await?;
        if self.enabled.load(Ordering::SeqCst) {
            Ok(Outlook {
                normal: outlook.prioritized.clone(),
                prioritized: outlook.prioritized,
            })
        } else {
            Ok(outlook)
        }
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        self.inner.keep_fresh().await
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        self.inner.status_html().await
    }

    fn worker_hosts(&self) -> Vec<String> {
        self.inner.worker_hosts()
    }

    async fn run(
        &self,
        ws: &crate::workspace::Workspace,
        prompt: &str,
        cap_tokens: Option<i64>,
        max_wall_s: i64,
        job_class: JobClass,
        resume_from: Option<&std::path::Path>,
    ) -> anyhow::Result<crate::types::RunResult> {
        self.inner
            .run(ws, prompt, cap_tokens, max_wall_s, job_class, resume_from)
            .await
    }
}

/// Deterministic backend for router tests: no window data, ever.
pub struct NullBackend;

#[async_trait]
impl Backend for NullBackend {
    async fn decide(&self, _anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let denied = Verdict::Denied {
            reason: "no window data -- deny until fresh".to_owned(),
            retry_at: None,
        };
        Ok(Outlook {
            normal: denied.clone(),
            prioritized: denied,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(r#"<div class="scv-note">No window data available</div>"#.to_owned())
    }

    /// None: a router test never reaches the network through it.
    fn worker_hosts(&self) -> Vec<String> {
        vec![]
    }

    async fn run(
        &self,
        _ws: &crate::workspace::Workspace,
        _prompt: &str,
        _cap_tokens: Option<i64>,
        _max_wall_s: i64,
        _job_class: JobClass,
        _resume_from: Option<&std::path::Path>,
    ) -> anyhow::Result<crate::types::RunResult> {
        anyhow::bail!("NullBackend cannot run workers")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SplitVerdictBackend;

    #[async_trait]
    impl Backend for SplitVerdictBackend {
        async fn decide(&self, _anticipated_tokens: i64) -> anyhow::Result<Outlook> {
            Ok(Outlook {
                normal: Verdict::Denied {
                    reason: "pacing ramp".to_owned(),
                    retry_at: Some(1_000.0),
                },
                prioritized: Verdict::Granted {
                    cap_tokens: Some(42),
                    reason: "prio override".to_owned(),
                },
            })
        }

        async fn keep_fresh(&self) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn status_html(&self) -> anyhow::Result<String> {
            Ok(String::new())
        }

        fn worker_hosts(&self) -> Vec<String> {
            Vec::new()
        }

        async fn run(
            &self,
            _ws: &crate::workspace::Workspace,
            _prompt: &str,
            _cap_tokens: Option<i64>,
            _max_wall_s: i64,
            _job_class: JobClass,
            _resume_from: Option<&std::path::Path>,
        ) -> anyhow::Result<crate::types::RunResult> {
            unreachable!("budget-mode test never starts a worker")
        }
    }

    #[tokio::test]
    async fn overdrive_promotes_the_provider_prioritized_verdict() -> anyhow::Result<()> {
        let enabled = Arc::new(AtomicBool::new(false));
        let backend = OverdriveBackend::new(Arc::new(SplitVerdictBackend), enabled.clone());

        let normal = backend.decide(1).await?;
        assert!(matches!(normal.normal, Verdict::Denied { .. }));
        assert!(matches!(normal.prioritized, Verdict::Granted { .. }));

        enabled.store(true, Ordering::SeqCst);
        let overdrive = backend.decide(1).await?;
        let expected = Verdict::Granted {
            cap_tokens: Some(42),
            reason: "prio override".to_owned(),
        };
        assert_eq!(overdrive.normal, expected);
        assert_eq!(overdrive.prioritized, expected);
        Ok(())
    }
}
