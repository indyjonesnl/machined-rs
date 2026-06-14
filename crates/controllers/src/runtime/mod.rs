//! Container-runtime controllers.

pub mod health;
pub mod pod;

pub use health::RuntimeHealthController;
pub use pod::PodController;

/// Namespace for runtime resources.
pub const NS: &str = "runtime";
