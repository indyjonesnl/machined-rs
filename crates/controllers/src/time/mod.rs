//! Time controllers.

pub mod sync;

pub use sync::TimeSyncController;

use std::fmt::Display;

use machined_runtime_core::Error;

/// Namespace for time resources.
pub const NS: &str = "runtime";

pub(crate) fn ctl<E: Display>(e: E) -> Error {
    Error::Controller(e.to_string())
}
