//! OMP LLM-provider quota definitions.
//!
//! A provider is data: how OMP names its usage limits and which quota
//! windows gate spending. The scavenging policy in `facade` loops over
//! [`Quota::windows`] and holds no provider- or window-specific branch.

use super::capacity::{
    FIVE_H_MS, WEEK_MS, WindowState, ramp_after_headroom, ramp_linear, retry_at_after_headroom,
    retry_at_linear,
};
use chrono::{DateTime, Datelike, Months, NaiveDate};
use std::collections::BTreeMap;

/// Capacity of a 5h window before calibration history exists: 200 k ≈
/// 10% of it (`facade._TOK_PER_FRAC_5H`).
const FIVE_H_FALLBACK_TOKENS: f64 = 2_000_000.0;
/// 5h / 7d period ratio ≈ 0.0297619.
const RATIO_5H_7D: f64 = FIVE_H_MS as f64 / WEEK_MS as f64;
/// A 7d window's fallback with an uncalibrated 5h window: 67.2 M.
const LONG_WINDOW_FALLBACK_TOKENS: f64 = FIVE_H_FALLBACK_TOKENS / RATIO_5H_7D;

static ANTHROPIC: Quota = Quota {
    windows: &[
        QuotaWindow {
            limit_id: "anthropic:7d",
            label: "7d window",
            period: Period::Fixed(WEEK_MS),
            pacing: Pacing::Linear,
            extra_prefix: Some("anthropic:7d:"),
            required: false,
            fallback_capacity: FallbackCapacity::Scaled {
                window: 1,
                ratio: RATIO_5H_7D,
            },
        },
        QuotaWindow {
            limit_id: "anthropic:5h",
            label: "5h window",
            period: Period::Fixed(FIVE_H_MS),
            pacing: Pacing::AfterHeadroom,
            extra_prefix: None,
            required: false,
            fallback_capacity: FallbackCapacity::Tokens(FIVE_H_FALLBACK_TOKENS),
        },
    ],
    exhausted_is_hard_stop: true,
    reads_other_limits: true,
};

static OPENAI_CODEX: Quota = Quota {
    windows: &[
        QuotaWindow {
            limit_id: "openai-codex:secondary",
            label: "7d window",
            period: Period::Fixed(WEEK_MS),
            pacing: Pacing::Linear,
            extra_prefix: None,
            required: false,
            fallback_capacity: FallbackCapacity::Scaled {
                window: 1,
                ratio: RATIO_5H_7D,
            },
        },
        QuotaWindow {
            limit_id: "openai-codex:primary",
            label: "5h window",
            period: Period::Fixed(FIVE_H_MS),
            pacing: Pacing::AfterHeadroom,
            extra_prefix: None,
            required: false,
            fallback_capacity: FallbackCapacity::Tokens(FIVE_H_FALLBACK_TOKENS),
        },
    ],
    // Codex's statuses are derived from the reported fraction and add
    // nothing to it.
    exhausted_is_hard_stop: false,
    reads_other_limits: true,
};

/// Copilot meters premium requests per UTC calendar month. Its chat and
/// completions limits never gate a hunter job: they are unlimited on paid
/// plans (where omp stops writing them, so their last rows would linger as
/// stale bars), and the free plan has no premium quota to run on anyway.
static GITHUB_COPILOT: Quota = Quota {
    windows: &[QuotaWindow {
        limit_id: "copilot:premium",
        label: "Monthly premium requests",
        period: Period::CalendarMonth,
        pacing: Pacing::Linear,
        extra_prefix: None,
        // No usage fraction (e.g. a 0/0 free plan) or reset is no quota,
        // not an unlimited one.
        required: true,
        fallback_capacity: FallbackCapacity::Tokens(LONG_WINDOW_FALLBACK_TOKENS),
    }],
    exhausted_is_hard_stop: true,
    reads_other_limits: false,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProvider {
    Anthropic,
    OpenAiCodex,
    GitHubCopilot,
}

impl LlmProvider {
    pub const ALL: [Self; 3] = [Self::Anthropic, Self::OpenAiCodex, Self::GitHubCopilot];

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.name() == value)
    }

    /// Every accepted provider name, quoted, for error messages.
    pub fn expected_names() -> String {
        Self::ALL
            .map(|provider| format!("'{}'", provider.name()))
            .join(" or ")
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiCodex => "openai-codex",
            Self::GitHubCopilot => "github-copilot",
        }
    }

    /// The API hosts a worker on this provider talks to: what the
    /// reachability gate probes when `backend.providerHosts` does not say
    /// otherwise. The endpoints omp itself calls for the provider.
    pub const fn api_hosts(self) -> &'static [&'static str] {
        match self {
            Self::Anthropic => &["api.anthropic.com"],
            // Codex on a ChatGPT plan is served from chatgpt.com/backend-api.
            Self::OpenAiCodex => &["chatgpt.com"],
            Self::GitHubCopilot => &["api.githubcopilot.com"],
        }
    }

    pub fn quota(self) -> &'static Quota {
        match self {
            Self::Anthropic => &ANTHROPIC,
            Self::OpenAiCodex => &OPENAI_CODEX,
            Self::GitHubCopilot => &GITHUB_COPILOT,
        }
    }

    /// Period of `limit_id` when it names any provider's quota window;
    /// None otherwise (Anthropic's per-model-class limits). Capacity
    /// calibration is keyed on this, and a daemon's window log may hold
    /// rows from whichever provider it ran with before.
    pub fn window_period(limit_id: &str) -> Option<Period> {
        Self::ALL
            .iter()
            .find_map(|provider| provider.quota().window(limit_id))
            .map(|window| window.period)
    }
}

/// The windows that gate one provider's spending.
#[derive(Debug)]
pub struct Quota {
    /// Longest period first. The order is policy: the first window that
    /// denies supplies the reason and `retry_at`, the longest window's
    /// fraction is the usage delta, and the shortest window's probe age
    /// decides whether omp must be re-probed.
    pub windows: &'static [QuotaWindow],
    /// Whether an `exhausted` status is a hard stop: the window counts as
    /// full and only its reset lifts the denial, even for prioritized work.
    pub exhausted_is_hard_stop: bool,
    /// Whether limits outside [`Self::windows`] are read at all. Read ones
    /// are displayed; only extra limits of a window gate.
    pub reads_other_limits: bool,
}

impl Quota {
    /// The quota window whose account-wide row is `limit_id`.
    pub fn window(&self, limit_id: &str) -> Option<&QuotaWindow> {
        self.windows
            .iter()
            .find(|window| window.limit_id == limit_id)
    }

    /// The quota window `limit_id` gates as: its account-wide row or one of
    /// its extra limits. None for limits that are only displayed.
    pub fn gating_window(&self, limit_id: &str) -> Option<&QuotaWindow> {
        self.windows.iter().find(|window| window.gates(limit_id))
    }

    pub fn is_hard_stop(&self, window: &WindowState) -> bool {
        self.exhausted_is_hard_stop && window.status.as_deref() == Some("exhausted")
    }

    pub fn effective_used(&self, window: &WindowState) -> Option<f64> {
        if self.is_hard_stop(window) {
            Some(1.0)
        } else {
            window.used_fraction
        }
    }

    /// Token capacity of `windows[index]`: its calibrated `estimate`
    /// when there is one, else its fallback.
    pub fn capacity(&self, index: usize, estimates: &[Option<f64>]) -> f64 {
        let estimate = estimates.get(index).copied().flatten();
        let fallback = || match self
            .windows
            .get(index)
            .map(|window| window.fallback_capacity)
        {
            Some(FallbackCapacity::Tokens(tokens)) => tokens,
            Some(FallbackCapacity::Scaled { window, ratio }) => {
                self.capacity(window, estimates) / ratio
            }
            None => 0.0,
        };
        estimate.unwrap_or_else(fallback)
    }
}

/// How long a window's cycle is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Fixed(i64),
    /// From 00:00 UTC on the first day of the month before the reset's
    /// month, exactly like `Store::estimate_capacity`'s SQL (`'start of
    /// month', '-1 month'`), so pacing and calibration agree even for a
    /// reset that is not on a month boundary.
    CalendarMonth,
}

impl Period {
    /// Length of the cycle ending at `resets_at`; None when a calendar
    /// month has no reset to measure from.
    pub fn length_ms(self, resets_at: Option<i64>) -> Option<i64> {
        match self {
            Self::Fixed(period_ms) => Some(period_ms),
            Self::CalendarMonth => {
                let reset = resets_at?;
                let reset_time = DateTime::from_timestamp_millis(reset)?;
                let start = NaiveDate::from_ymd_opt(reset_time.year(), reset_time.month(), 1)?
                    .checked_sub_months(Months::new(1))?
                    .and_hms_opt(0, 0, 0)?
                    .and_utc();
                Some(reset - start.timestamp_millis())
            }
        }
    }

    /// The period an expired cycle can be rolled forward by. A calendar
    /// month has none: rolling it would invent unused quota without
    /// provider data.
    pub const fn fixed_ms(self) -> Option<i64> {
        match self {
            Self::Fixed(period_ms) => Some(period_ms),
            Self::CalendarMonth => None,
        }
    }
}

/// How a window's allowance grows over its cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pacing {
    /// [`ramp_linear`]: the elapsed fraction of the cycle.
    Linear,
    /// [`ramp_after_headroom`]: nothing for the first `HEADROOM_MS`, then
    /// linear. Without an active cycle the window does not gate.
    AfterHeadroom,
}

/// A window's token capacity before calibration history exists.
#[derive(Debug, Clone, Copy)]
pub enum FallbackCapacity {
    Tokens(f64),
    /// `windows[window]`'s capacity divided by `ratio` (its period over
    /// this one's), so a calibrated short window scales up.
    Scaled {
        window: usize,
        ratio: f64,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct QuotaWindow {
    /// The account-wide row: calibration, reservation and freshness key.
    pub limit_id: &'static str,
    /// Status-page label of the account-wide row.
    pub label: &'static str,
    pub period: Period,
    pub pacing: Pacing,
    /// Extra limits `<prefix><class>` that gate like this window, against
    /// its reservation and capacity (Anthropic's per-model-class limits).
    pub extra_prefix: Option<&'static str>,
    /// Whether work is refused, prioritized work included, while this
    /// window's account row is missing, has no used fraction, or has no
    /// future reset. Otherwise such a window simply does not gate.
    pub required: bool,
    pub fallback_capacity: FallbackCapacity,
}

impl QuotaWindow {
    pub fn gates(&self, limit_id: &str) -> bool {
        limit_id == self.limit_id
            || self
                .extra_prefix
                .is_some_and(|prefix| limit_id.starts_with(prefix))
    }

    /// Allowed used fraction at `now_ms`; None when the window is not
    /// active and therefore does not gate.
    pub fn ramp(&self, resets_at: Option<i64>, now_ms: i64) -> Option<f64> {
        let period_ms = self.period.length_ms(resets_at)?;
        match self.pacing {
            Pacing::Linear => Some(ramp_linear(resets_at, period_ms, now_ms)),
            Pacing::AfterHeadroom => ramp_after_headroom(resets_at, period_ms, now_ms),
        }
    }

    /// When the ramp reaches `effective_used`: the inverse of [`Self::ramp`].
    pub fn retry_at(&self, resets_at: Option<i64>, effective_used: f64) -> Option<f64> {
        let period_ms = self.period.length_ms(resets_at)?;
        match self.pacing {
            Pacing::Linear => retry_at_linear(resets_at, period_ms, effective_used),
            Pacing::AfterHeadroom => retry_at_after_headroom(resets_at, period_ms, effective_used),
        }
    }

    /// Whether this window is [`Self::required`] but its account row in
    /// `windows` is missing, has no used fraction, or no future reset at
    /// `now_ms`. A reset exactly at `now_ms` has expired.
    pub fn is_missing(&self, windows: &BTreeMap<String, WindowState>, now_ms: i64) -> bool {
        self.required
            && !windows.get(self.limit_id).is_some_and(|row| {
                row.used_fraction.is_some() && row.resets_at.is_some_and(|reset| reset > now_ms)
            })
    }
}
