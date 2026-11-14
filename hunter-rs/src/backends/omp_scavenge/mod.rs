//! omp-scavenge backend — spends idle subscription capacity by
//! scavenging omp's local usage mirror. Port of
//! `hunter/backends/omp_scavenge`/ per BACKEND-CONTRACT.md §2.
//!
//! `provider`: each provider's quota windows, as data. `capacity`: the
//! window reading and ramp math. `facade`: `decide`, `keep_fresh` and
//! `status_html`, one loop over the provider's windows. `harness`:
//! `run_worker`, the usage-delta sandwich around it, and `Backend::run`.
//! `gateway`: the auth broker and gateway the daemon runs, and the
//! per-run gateway token a worker reaches its provider with.
pub mod capacity;
mod facade;
pub mod gateway;
pub mod harness;
pub mod provider;

pub use capacity::default_agent_db;
pub use facade::OmpScavengeBackend;
pub use provider::LlmProvider;
// The status renderer and its inputs: a seam so the exact markup of
// BACKEND-CONTRACT.md §2.3 can be pinned by snapshot without a clock or
// a database. See tests/status_html_test.rs.
pub use facade::{NO_WINDOW_DATA, StatusInputs, render_status};
