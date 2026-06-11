//! `Runner` backend that forks/execs a host process via `tokio::process`.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::process::{Child, Command};
use tracing::warn;

use crate::runner::{RunOutcome, Runner, RunnerError};

pub struct ProcessRunner {
    id: String,
    command: Vec<String>,
    child: Option<Child>,
    /// Shared view of the live child PID (None when not running). The manager
    /// reads it to deliver SIGTERM during graceful stop.
    pid_slot: Arc<Mutex<Option<u32>>>,
}

impl ProcessRunner {
    /// `command[0]` is the program, the rest are args.
    pub fn new(id: impl Into<String>, command: Vec<String>) -> Self {
        Self {
            id: id.into(),
            command,
            child: None,
            pid_slot: Arc::new(Mutex::new(None)),
        }
    }

    /// Handle the manager uses to signal the live child.
    pub fn pid_slot(&self) -> Arc<Mutex<Option<u32>>> {
        self.pid_slot.clone()
    }
}

#[async_trait]
impl Runner for ProcessRunner {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&mut self) -> crate::runner::Result<RunOutcome> {
        let (program, args) = self
            .command
            .split_first()
            .ok_or_else(|| RunnerError::Other(format!("service {} has empty command", self.id)))?;

        let child = Command::new(program)
            .args(args)
            // Kill the child if its task is aborted/dropped, so shutdown
            // (which aborts the supervising task) does not orphan the process
            // onto PID 1. Graceful SIGTERM + grace timeout lands in M5.
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| RunnerError::Spawn {
                id: self.id.clone(),
                source,
            })?;
        self.child = Some(child);
        *self.pid_slot.lock().unwrap() = self.child.as_ref().unwrap().id();

        let status = self
            .child
            .as_mut()
            .unwrap()
            .wait()
            .await
            .map_err(|e| RunnerError::Other(format!("wait {}: {e}", self.id)))?;
        self.child = None;
        *self.pid_slot.lock().unwrap() = None;

        Ok(if status.success() {
            RunOutcome::Success
        } else {
            RunOutcome::Failure
        })
    }

    async fn stop(&mut self) -> crate::runner::Result<()> {
        if let Some(child) = self.child.as_mut() {
            // tokio's start_kill sends SIGKILL; for M1 that is acceptable.
            // M5 replaces this with SIGTERM + grace timeout + SIGKILL.
            if let Err(e) = child.start_kill() {
                warn!(service = %self.id, "kill failed: {e}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_success_exits_zero() {
        let mut r = ProcessRunner::new("ok", vec!["true".into()]);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Success);
    }

    #[tokio::test]
    async fn run_failure_exits_nonzero() {
        let mut r = ProcessRunner::new("bad", vec!["false".into()]);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Failure);
    }

    #[tokio::test]
    async fn empty_command_errors() {
        let mut r = ProcessRunner::new("empty", vec![]);
        assert!(r.run().await.is_err());
    }

    #[tokio::test]
    async fn missing_program_errors() {
        let mut r = ProcessRunner::new("nope", vec!["/no/such/binary".into()]);
        assert!(matches!(r.run().await, Err(RunnerError::Spawn { .. })));
    }
}
