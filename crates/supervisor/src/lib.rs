//! Service supervision: run services via pluggable runners, drive them through
//! a lifecycle, and reflect their state as `ServiceStatus` resources.

pub mod process;
pub mod runner;

pub use process::ProcessRunner;
pub use runner::{RunOutcome, Runner, RunnerError};

pub mod restart;
pub use restart::{Policy, RestartRunner};

pub mod manager;
pub mod readiness;
pub mod service;

pub use manager::{start_order, ServiceManager};
pub use readiness::{DefaultReadiness, ReadinessCheck};
pub use service::{publish_status, run_service};
