//! Block controllers. M2b-1: read-only discovery.

pub mod discovery;

pub use discovery::DiskDiscoveryController;

use std::fmt::Display;

use machined_runtime_core::Error;

/// Namespace for block resources.
pub const NS: &str = "block";

/// Map a backend error into a runtime-core controller error.
pub(crate) fn ctl<E: Display>(e: E) -> Error {
    Error::Controller(e.to_string())
}
