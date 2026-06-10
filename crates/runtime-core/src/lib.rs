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
pub use runtime::{Controller, Input, InputKind, Output, OutputKind, ReconcileCtx, Runtime};
pub use state::State;
pub use watch::{Event, EventKind};
