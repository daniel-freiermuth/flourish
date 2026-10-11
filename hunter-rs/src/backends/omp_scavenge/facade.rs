//! `OmpScavengeBackend` facade (facade.py, BACKEND-CONTRACT.md §2.2-2.3).
//! Bundles the harness, the window accounting and the scavenging policy
//! behind the single `Backend` the scheduler sees.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use super::capacity::{self, HEADROOM_MS, WindowState};
use super::provider::{LlmProvider, Pacing};

use crate::backend::SpendLedger;
use crate::backend::{Backend, JobClass, Outlook, Prober, Verdict};
use crate::config::Config;

/// All omp-scavenge vocabulary (quota windows, ramp math, probe
/// staleness) is confined to this type; which windows a provider has and
/// how each paces is data in `LlmProvider::quota`.
pub struct OmpScavengeBackend {
    pub cfg: Config,
    pub ledger: Arc<dyn SpendLedger>,
    /// omp's usage mirror; `default_agent_db()` in production, a fixture in
    /// tests.
    pub agent_db: PathBuf,
    /// `keep_fresh`'s `omp usage` subprocess seam.
    pub prober: Arc<dyn Prober>,
}

// ---- private helpers (facade.py functions) --------------------------------

/// html.escape equivalent: & < > " ' (`facade._esc`).
///
/// Free functions rather than associated ones: they are pure, and
/// `render_status` needs them without a backend instance.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Format token count (`facade.OmpScavengeBackend._fmt_tokens`).
fn fmt_tokens(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.0}k", n / 1_000.0)
    } else {
        format!("{}", n as i64)
    }
}

/// Token count as a fraction of a window's capacity. A capacity that is
/// not positive carries no meaningful fraction.
fn as_fraction(tokens: i64, capacity: f64) -> f64 {
    if capacity > 0.0 {
        tokens as f64 / capacity
    } else {
        0.0
    }
}

/// One quota window's inflight reservation for one `decide` call, as a
/// fraction of the window's capacity. Field names follow the Python
/// dataclass.
///
/// Two reservations, because the gate and the cap ask different questions
/// of the same number. `gate` counts `anticipated` in: the gate asks "if
/// this job also runs, does the window cross its ramp?", and the job's own
/// cost belongs in that answer. `budget` leaves it out, because the
/// headroom a grant computes IS the job's budget — charging `anticipated`
/// against the reservation and then handing the job what remains counts
/// that cost twice, giving it `headroom - anticipated` tokens to do work
/// worth `anticipated`. A job that clears the gate by a hair would then be
/// capped far below its own session floor and killed by the watchdog
/// having produced nothing, while the tokens it did spend still count
/// against the window. Taking every cap from `budget` makes
/// `cap >= anticipated` hold whenever the gate grants.
#[derive(Clone, Copy)]
struct Reservation {
    /// Gate view: running + finished-since-probe + `anticipated`.
    gate: f64,
    /// Cap view: the same, without `anticipated`.
    budget: f64,
    /// Capacity in tokens the fractions were taken against.
    capacity: f64,
}

impl Reservation {
    /// Fraction → token cap (`facade.OmpScavengeBackend._frac_to_tokens`).
    fn tokens(self, frac: f64) -> i64 {
        (frac * self.capacity) as i64
    }
}

impl OmpScavengeBackend {
    /// Inflight reservation per quota window, in `Quota::windows` order
    /// (`facade.OmpScavengeBackend._unaccounted_fraction` / `_reservation`).
    /// See [`Reservation`] for why each has two views.
    ///
    /// Each window's capacity is its own `estimate_capacity`, else its
    /// fallback (a constant, or a shorter window's capacity scaled by the
    /// period ratio) while there is not yet history to estimate from.
    async fn reservations(
        &self,
        windows: &BTreeMap<String, WindowState>,
        anticipated: i64,
    ) -> anyhow::Result<Vec<Reservation>> {
        let quota = self.cfg.llm_provider.quota();
        let running = self.ledger.running_estimate().await?;
        let fallback_probe = windows.values().map(|w| w.recorded_at).min().unwrap_or(0);
        let base = running + anticipated;

        let mut estimates = Vec::with_capacity(quota.windows.len());
        for window in quota.windows {
            estimates.push(self.ledger.estimate_capacity(window.limit_id).await?);
        }

        let mut reservations = Vec::with_capacity(quota.windows.len());
        for (index, window) in quota.windows.iter().enumerate() {
            // Per-window probe time, never a shared minimum: spend is
            // unaccounted only since the probe that saw this window.
            let probe_at = windows
                .get(window.limit_id)
                .map_or(fallback_probe, |w| w.recorded_at);
            let unaccounted = base + self.ledger.finished_since(probe_at).await?;
            let capacity = quota.capacity(index, &estimates);
            reservations.push(Reservation {
                gate: as_fraction(unaccounted, capacity),
                budget: as_fraction(unaccounted - anticipated, capacity),
                capacity,
            });
        }
        Ok(reservations)
    }

    /// Single verdict (`facade.OmpScavengeBackend._decide_inner`): every
    /// row each quota window gates, longest window first.
    fn decide_inner(
        provider: LlmProvider,
        windows: &BTreeMap<String, WindowState>,
        reservations: &[Reservation],
        prio: bool,
        now_ms: i64,
    ) -> Verdict {
        if windows.is_empty() {
            return Verdict::Denied {
                reason: "no window data -- deny until fresh".to_owned(),
                retry_at: None,
            };
        }

        let quota = provider.quota();
        for (quota_window, resv) in quota.windows.iter().zip(reservations) {
            for (lid, window) in windows.iter().filter(|(lid, _)| quota_window.gates(lid)) {
                if let Some(resets_at) = window.resets_at
                    && resets_at > 0
                    && resets_at <= now_ms
                {
                    continue;
                }
                let Some(reported) = quota.effective_used(window) else {
                    continue;
                };
                // None: no active window, so it does not gate (the next
                // job opens it).
                let Some(allowed) = quota_window.ramp(window.resets_at, now_ms) else {
                    continue;
                };
                let effective_used = reported + resv.gate;
                if effective_used < allowed {
                    continue;
                }

                let exhausted = quota.is_hard_stop(window);
                let retry_at = if exhausted {
                    window.resets_at.map(|reset| reset as f64)
                } else {
                    quota_window.retry_at(window.resets_at, effective_used)
                };
                let reason = format!(
                    "{lid}: used {reported:.2} + unaccounted {res:.2} = {effective_used:.2} >= ramp {allowed:.2}",
                    res = resv.gate
                );
                if prio && !exhausted {
                    // Headroom for THIS job, so the reservation it is
                    // measured against must not already contain it.
                    let cap = resv.tokens((1.0 - (reported + resv.budget)).max(0.0));
                    if cap > 0 {
                        return Verdict::Granted {
                            cap_tokens: Some(cap),
                            reason: format!("prio override ({reason})"),
                        };
                    }
                }
                return Verdict::Denied { reason, retry_at };
            }
        }

        // Gate and cap use the same decision-time snapshot.
        let headroom = Self::compute_headroom(provider, windows, reservations, prio, now_ms);
        Verdict::Granted {
            cap_tokens: headroom,
            reason: "ok".to_owned(),
        }
    }

    /// Min headroom in tokens across every gating row
    /// (`facade.OmpScavengeBackend._compute_headroom`).
    ///
    /// Measured against `budget` (see [`Reservation`]): this headroom is
    /// what the anticipated job is allowed to spend, not what is left once
    /// it has spent it.
    fn compute_headroom(
        provider: LlmProvider,
        windows: &BTreeMap<String, WindowState>,
        reservations: &[Reservation],
        prio: bool,
        now_ms: i64,
    ) -> Option<i64> {
        let quota = provider.quota();
        let mut caps: Vec<i64> = Vec::new();
        for (quota_window, resv) in quota.windows.iter().zip(reservations) {
            for (_, window) in windows.iter().filter(|(lid, _)| quota_window.gates(lid)) {
                let Some(used) = quota.effective_used(window) else {
                    continue;
                };
                let Some(ramp) = quota_window.ramp(window.resets_at, now_ms) else {
                    continue;
                };
                let ceiling = if prio { 1.0 } else { ramp };
                caps.push(resv.tokens((ceiling - used - resv.budget).max(0.0)));
            }
        }
        caps.into_iter().min()
    }

    /// Log observations and calibrate the selected provider's quota windows.
    async fn observe(&self, windows: &BTreeMap<String, WindowState>) -> anyhow::Result<()> {
        let now = crate::util::now_ms();
        let quota = self.cfg.llm_provider.quota();

        for window in windows.values() {
            let horizon = quota
                .window(&window.limit_id)
                .and_then(|w| w.period.length_ms(window.resets_at));

            if let Some(period_ms) = horizon
                && let (Some(resets), Some(used_fraction)) =
                    (window.resets_at, window.used_fraction)
                && let Some((previous_observation, previous_fraction)) = self
                    .ledger
                    .last_window_observation(&window.limit_id, resets)
                    .await?
                && used_fraction > previous_fraction
                && (now - previous_observation) <= period_ms
            {
                let tokens = self
                    .ledger
                    .finished_between(previous_observation, now)
                    .await?;
                if tokens > 0 {
                    self.ledger
                        .record_calibration_sample(
                            &window.limit_id,
                            Some(resets),
                            used_fraction - previous_fraction,
                            tokens,
                        )
                        .await?;
                }
            }

            self.ledger
                .log_window_observation(
                    &window.limit_id,
                    window.used_fraction,
                    window.status.as_deref(),
                    window.resets_at,
                    window.age_s,
                )
                .await?;
        }

        Ok(())
    }

    /// Max `used_fraction` across the rows the longest quota window gates,
    /// or None (`facade.OmpScavengeBackend._usage_snapshot`).
    /// Blocking — meant for `spawn_blocking`.
    fn _usage_snapshot_sync(agent_db: &std::path::Path, provider: LlmProvider) -> Option<f64> {
        let now = crate::util::now_ms();
        let longest = provider.quota().windows.first()?;
        let windows = capacity::read_windows(agent_db, provider, now);
        windows
            .iter()
            .filter(|(lid, _)| longest.gates(lid))
            .filter_map(|(_, w)| w.used_fraction)
            .reduce(f64::max)
    }
}

// ---- public testing seam --------------------------------------------------

impl OmpScavengeBackend {
    /// Runs decide logic on a usage snapshot at an explicit decision time.
    /// `Backend::decide` supplies the clock; tests can exercise exact resets.
    pub async fn decide_with_windows(
        &self,
        windows: &BTreeMap<String, WindowState>,
        anticipated_tokens: i64,
        now: i64,
    ) -> anyhow::Result<Outlook> {
        let quota = self.cfg.llm_provider.quota();
        if let Some(missing) = quota
            .windows
            .iter()
            .find(|window| window.is_missing(windows, now))
        {
            let denied = Verdict::Denied {
                reason: format!("{} unknown -- deny until fresh", missing.limit_id),
                retry_at: None,
            };
            return Ok(Outlook {
                normal: denied.clone(),
                prioritized: denied,
            });
        }
        let resv = self.reservations(windows, anticipated_tokens).await?;
        let normal = Self::decide_inner(self.cfg.llm_provider, windows, &resv, false, now);

        let prioritized = match &normal {
            Verdict::Granted {
                cap_tokens: normal_cap,
                ..
            } => {
                // Normal granted → prioritized may upgrade its headroom.
                let prio_headroom =
                    Self::compute_headroom(self.cfg.llm_provider, windows, &resv, true, now);
                match (prio_headroom, normal_cap) {
                    (Some(ph), None) => Verdict::Granted {
                        cap_tokens: Some(ph),
                        reason: "ok".to_owned(),
                    },
                    (Some(ph), Some(nc)) if ph > *nc => Verdict::Granted {
                        cap_tokens: Some(ph),
                        reason: "ok".to_owned(),
                    },
                    _ => normal.clone(),
                }
            }
            Verdict::Denied { .. } => {
                // Normal denied → compute prioritized with prio=true.
                Self::decide_inner(self.cfg.llm_provider, windows, &resv, true, now)
            }
        };

        Ok(Outlook {
            normal,
            prioritized,
        })
    }

    /// Staleness predicate behind `keep_fresh`'s probe gate: fresh iff the
    /// provider's shortest quota window exists and its age is within
    /// `stale_after_s`, inclusive — an age landing exactly on the threshold
    /// is still fresh. A missing shortest window (or no windows at all) is
    /// not fresh.
    ///
    /// Split out from `keep_fresh` so the boundary is observable without
    /// the wall-clock read that derives `age_s`.
    pub fn is_fresh(&self, windows: &BTreeMap<String, WindowState>) -> bool {
        self.cfg
            .llm_provider
            .quota()
            .windows
            .last()
            .and_then(|shortest| windows.get(shortest.limit_id))
            .is_some_and(|shortest| shortest.age_s <= self.cfg.stale_after_s)
    }
}

/// Returned verbatim when there is no window data at all.
pub const NO_WINDOW_DATA: &str = r#"<div class="scv-note">No window data available</div>"#;

/// Everything [`render_status`] needs, resolved by `status_html` from the
/// clock, `agent.db` and the ledger.
///
/// Split out so the markup can be pinned by snapshot. `status_html`
/// otherwise reads the wall clock and two databases, which makes its
/// output — a precisely specified HTML fragment the UI injects with
/// `{@html}` — impossible to assert on. Same seam as
/// `decide_with_windows` and `is_fresh` elsewhere in this file.
pub struct StatusInputs {
    pub now_ms: i64,
    pub provider: LlmProvider,
    pub windows: BTreeMap<String, WindowState>,
    /// Quota window `limit_id` -> in-flight reservation (its gate
    /// fraction). Missing windows count as 0.
    pub unaccounted: BTreeMap<&'static str, f64>,
    /// `limit_id` -> estimated window capacity in tokens, when known.
    pub capacities: BTreeMap<String, Option<f64>>,
    pub stale_after_s: f64,
}

/// Render the budget window bars (BACKEND-CONTRACT.md §2.3).
///
/// The class names are load-bearing against the `:global(.scv-*)` rules in
/// `hunter/ui-svelte/src/pages/StatusPage.svelte` -- not `hunter/ui/`, which
/// is generated build output -- and the UI injects this as raw HTML, so the
/// markup is the contract. A new class needs a rule there too. Pinned by
/// `tests/status_html_test.rs`.
#[allow(
    clippy::too_many_lines,
    reason = "one HTML template expressed as sequential `write!` calls; the \
              markup is the contract, so it has to be readable top to \
              bottom as the document it produces"
)]
#[must_use]
pub fn render_status(inputs: &StatusInputs) -> String {
    if inputs.windows.is_empty() {
        return NO_WINDOW_DATA.to_owned();
    }
    let mut fragments: Vec<String> = Vec::new();

    let quota = inputs.provider.quota();
    for (lid, w) in &inputs.windows {
        let quota_window = quota.gating_window(lid);
        let label = match quota_window {
            Some(quota_window) if quota_window.limit_id == lid => quota_window.label.to_owned(),
            _ => {
                // Extra limits (Anthropic's `anthropic:7d:<class>`, Codex's
                // `openai-codex:<model>:<window>`) keep their id minus the
                // provider prefix.
                let name = lid
                    .strip_prefix(inputs.provider.name())
                    .and_then(|rest| rest.strip_prefix(':'))
                    .unwrap_or(lid);
                format!("{name} window")
            }
        };
        let used_pct = match w.used_fraction {
            Some(f) => format!("{:.0}%", f * 100.0),
            None => "?".to_owned(),
        };

        // `elapsed_ms`: time into an active after-headroom window, which is
        // what the headroom note needs.
        let (unacct, ramp_val, elapsed_ms): (f64, Option<f64>, Option<i64>) = match quota_window {
            Some(quota_window) => {
                let ramp = quota_window.ramp(w.resets_at, inputs.now_ms);
                let period = quota_window.period.length_ms(w.resets_at);
                // A ramp exists only while the window is active, i.e.
                // `resets_at` is in the future.
                let elapsed_ms = match (quota_window.pacing, ramp, w.resets_at, period) {
                    (Pacing::AfterHeadroom, Some(_), Some(reset), Some(period)) => {
                        Some(period - (reset - inputs.now_ms))
                    }
                    _ => None,
                };
                let unacct = inputs
                    .unaccounted
                    .get(quota_window.limit_id)
                    .copied()
                    .unwrap_or(0.0);
                (unacct, ramp, elapsed_ms)
            }
            None => (0.0, None, None),
        };

        // fill_pct: round-half-to-even (Python's round()), clamped 0..100.
        let fill_pct =
            ((w.used_fraction.unwrap_or(0.0) * 100.0).round_ties_even() as i64).clamp(0, 100) as u8;
        let soft_pct =
            ((unacct * 100.0).round_ties_even() as i64).clamp(0, 100 - i64::from(fill_pct)) as u8;
        let ramp_pct: Option<u8> =
            ramp_val.map(|r| ((r * 100.0).round_ties_even() as i64).clamp(0, 100) as u8);

        let ramp_for_avail = ramp_val.unwrap_or(1.0);
        let avail_frac = (ramp_for_avail - w.used_fraction.unwrap_or(0.0) - unacct).max(0.0);
        let avail_pct_str = format!("{:.0}%", avail_frac * 100.0);

        let cap = inputs.capacities.get(lid).copied().flatten();
        let avail_tok = cap.filter(|&c| c > 0.0).map(|c| avail_frac * c);

        let avail_str = if w.used_fraction.is_none() {
            String::new()
        } else if let Some(tok) = avail_tok {
            format!(
                " \u{00b7} {} avail (~{} tok)",
                avail_pct_str,
                fmt_tokens(tok)
            )
        } else {
            format!(" \u{00b7} {avail_pct_str} avail")
        };

        let is_stale = w.age_s > inputs.stale_after_s;
        let is_exhausted =
            w.status.as_deref() == Some("exhausted") || w.used_fraction.is_some_and(|f| f >= 1.0);
        let tone = if is_stale {
            "stale"
        } else if is_exhausted {
            "bad"
        } else {
            "ok"
        };

        let probe_age = format!("{:.0}m ago", w.age_s / 60.0);

        let reset_str = match w.resets_at {
            Some(r) => {
                let remain_s = (r - inputs.now_ms) as f64 / 1000.0;
                if remain_s > 0.0 {
                    let h = (remain_s / 3600.0) as i64;
                    let m = ((remain_s % 3600.0) / 60.0) as i64;
                    let countdown = if h > 0 {
                        format!("{h}h{m:02}m")
                    } else {
                        format!("{m}m")
                    };
                    // Emit <time data-ms="..."> so the client renders
                    // in the *browser's* local timezone, not the server's.
                    format!("resets <time data-ms=\"{r}\"></time> ({countdown})")
                } else {
                    "resetting".to_owned()
                }
            }
            None => "reset unknown".to_owned(),
        };

        // Counts down the window's first HEADROOM_MS. Never for a reset more
        // than one window ahead (negative elapsed), which would otherwise
        // read as more headroom than there is.
        let headroom_str = match elapsed_ms {
            Some(elapsed_ms) if (1..HEADROOM_MS).contains(&elapsed_ms) => format!(
                " \u{00b7} headroom {}m",
                (HEADROOM_MS - elapsed_ms) / 60_000
            ),
            _ => String::new(),
        };

        let unacct_str = if unacct > 0.005 {
            format!(" +{:.0}% in flight", unacct * 100.0)
        } else {
            String::new()
        };

        let marker = match ramp_pct {
            Some(rp) if rp > 0 => {
                format!(r#"<i class="scv-ramp" style="left:{rp}%"></i>"#)
            }
            _ => String::new(),
        };

        let stale_marker = if is_stale {
            " \u{26a0}\u{fe0f}stale"
        } else {
            ""
        };

        // Per-window fragment: exact HTML from BACKEND-CONTRACT.md §2.3.
        let fragment = format!(
            "<div class=\"scv-win\">\
                <div class=\"scv-lab\">\
                <b>{}</b>\
                <span>{} used{}{}{}</span>\
                </div>\
                <div class=\"scv-bar\">\
                <i class=\"scv-fill scv-{}\" style=\"width:{}%\"></i>\
                <i class=\"scv-soft\" style=\"width:{}%\"></i>\
                {}</div>\
                <div class=\"scv-sub\">{}{} \u{00b7} probed {}</div>\
                </div>",
            esc(&label),
            esc(&used_pct),
            esc(&unacct_str),
            esc(&avail_str),
            stale_marker,
            tone,
            fill_pct,
            soft_pct,
            marker,
            reset_str, // NOT escaped: contains <time data-ms="..."> for client-side TZ formatting
            esc(&headroom_str),
            esc(&probe_age),
        );

        fragments.push(fragment);
    }

    fragments.join("\n")
}

#[async_trait]
impl Backend for OmpScavengeBackend {
    async fn decide(&self, anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let now = crate::util::now_ms();
        let db = self.agent_db.clone();
        let provider = self.cfg.llm_provider;
        let windows =
            tokio::task::spawn_blocking(move || capacity::read_windows(&db, provider, now)).await?;
        // The same `now` as the read: a reset landing between two clock
        // reads would leave an un-rolled window that `decide_inner` skips.
        self.decide_with_windows(&windows, anticipated_tokens, now)
            .await
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        let now = crate::util::now_ms();
        let db = self.agent_db.clone();
        let provider = self.cfg.llm_provider;
        let windows =
            tokio::task::spawn_blocking(move || capacity::read_windows(&db, provider, now)).await?;

        self.observe(&windows).await?;
        if self.is_fresh(&windows) {
            return Ok(false);
        }

        {
            let omp = self.cfg.omp_bin.clone();
            let prober = self.prober.clone();
            let provider_name = self.cfg.llm_provider.name();
            tokio::task::spawn_blocking(move || {
                prober.run(
                    &[
                        omp.as_str(),
                        "usage",
                        "invalidate",
                        "--provider",
                        provider_name,
                    ],
                    15,
                );
            })
            .await?;
        }

        let omp = self.cfg.omp_bin.clone();
        let prober = self.prober.clone();
        let provider_name = self.cfg.llm_provider.name();
        let (rc, _out) = tokio::task::spawn_blocking(move || {
            prober.run(&[omp.as_str(), "usage", "--provider", provider_name], 30)
        })
        .await?;

        Ok(rc == 0)
    }

    /// `backend.providerHosts` when the operator set it, else the hosts
    /// omp calls for the configured provider.
    fn worker_hosts(&self) -> Vec<String> {
        self.cfg.provider_hosts.clone().unwrap_or_else(|| {
            self.cfg
                .llm_provider
                .api_hosts()
                .iter()
                .map(|&h| h.to_owned())
                .collect()
        })
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        let now_ms = crate::util::now_ms();
        let db = self.agent_db.clone();
        let provider = self.cfg.llm_provider;
        let windows =
            tokio::task::spawn_blocking(move || capacity::read_windows(&db, provider, now_ms))
                .await?;

        if windows.is_empty() {
            return Ok(NO_WINDOW_DATA.to_owned());
        }

        // anticipated=0: bars show observable state, not gate's hypothetical.
        let resv = self.reservations(&windows, 0).await?;
        let mut capacities = BTreeMap::new();
        for lid in windows.keys() {
            capacities.insert(lid.clone(), self.ledger.estimate_capacity(lid).await?);
        }

        Ok(render_status(&StatusInputs {
            now_ms,
            provider: self.cfg.llm_provider,
            windows,
            unaccounted: self
                .cfg
                .llm_provider
                .quota()
                .windows
                .iter()
                .zip(&resv)
                .map(|(quota_window, resv)| (quota_window.limit_id, resv.gate))
                .collect(),
            capacities,
            stale_after_s: self.cfg.stale_after_s,
        }))
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
        let pre = {
            let db = self.agent_db.clone();
            let provider = self.cfg.llm_provider;
            tokio::task::spawn_blocking(move || Self::_usage_snapshot_sync(&db, provider)).await?
        };

        let model = self
            .cfg
            .model_for(job_class.as_str())
            .map(std::borrow::ToOwned::to_owned);
        let cfg = self.cfg.clone();
        let ws_owned = ws.clone();
        let prompt_owned = prompt.to_owned();
        let resume_owned = resume_from.map(std::path::Path::to_owned);
        let mut rr = tokio::task::spawn_blocking(move || {
            super::harness::run_worker(
                &cfg,
                &ws_owned,
                &prompt_owned,
                cap_tokens,
                max_wall_s,
                model.as_deref(),
                resume_owned.as_deref(),
            )
        })
        .await?;

        let post = {
            let db = self.agent_db.clone();
            let provider = self.cfg.llm_provider;
            tokio::task::spawn_blocking(move || Self::_usage_snapshot_sync(&db, provider)).await?
        };

        rr.usage_delta = match (pre, post) {
            (Some(before), Some(after)) => Some(after - before),
            _ => None,
        };
        Ok(rr)
    }
}
