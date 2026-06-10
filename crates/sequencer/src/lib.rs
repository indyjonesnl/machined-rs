//! Lifecycle sequencing: ordered, idempotent tasks grouped into phases.

pub mod boot;
pub mod shutdown;
pub mod task;

pub use boot::boot_sequence;
pub use shutdown::shutdown_sequence;
pub use task::{Phase, PhaseList, SequencerCtx, Task, TaskError};
