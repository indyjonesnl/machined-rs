//! The `Runner` abstraction: one backend that can start, await, and stop a
//! single service instance.

use async_trait::async_trait;

#[derive(thiserror::Error, Debug)]
pub enum RunnerError {
    #[error("spawning service {id}: {source}")]
    Spawn {
        id: String,
        #[source]
        source: std::io::Error,
    },
    #[error("service {0} is not running")]
    NotRunning(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, RunnerError>;

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// Process exited 0.
    Success,
    /// Process exited non-zero or by signal.
    Failure,
    /// `stop()` was requested.
    Stopped,
}

/// A startable/stoppable service backend. One `Runner` drives one instance.
#[async_trait]
pub trait Runner: Send {
    /// Human-readable id for logging/status.
    fn id(&self) -> &str;
    /// Start the instance and block until it exits or `stop` is called.
    async fn run(&mut self) -> Result<RunOutcome>;
    /// Request a graceful stop of a running instance.
    async fn stop(&mut self) -> Result<()>;
}

/// Allow driving a runner through a mutable borrow (e.g. per-attempt runs in a
/// supervision loop that retains ownership across attempts).
#[async_trait]
impl<R: Runner> Runner for &mut R {
    fn id(&self) -> &str {
        (**self).id()
    }
    async fn run(&mut self) -> Result<RunOutcome> {
        (**self).run().await
    }
    async fn stop(&mut self) -> Result<()> {
        (**self).stop().await
    }
}
