//! Generic, statically-typed reconcile runtime for machined-rs.
//!
//! Provides an in-memory resource [`State`] store with COSI semantics
//! (versioned CAS updates, finalizers, owner refs, teardown), a broadcast
//! watch bus, and a [`Runtime`] that drives one reconcile loop per
//! registered [`Controller`].

pub mod error;
pub mod runtime;
pub mod state;
pub mod watch;

pub use error::{Error, Result};
// pub use runtime::{...};  // restored in Task 6
// pub use state::State;    // restored in Task 5
// pub use watch::{...};    // restored in Task 4
