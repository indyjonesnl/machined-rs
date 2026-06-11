//! Container-runtime controllers.

pub mod health;

pub use health::RuntimeHealthController;

/// Namespace for runtime resources.
pub const NS: &str = "runtime";
