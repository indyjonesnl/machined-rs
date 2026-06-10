//! A `Runner` decorator that re-runs the inner runner according to a policy.

use async_trait::async_trait;
use std::time::Duration;
use tracing::info;

use crate::runner::{RunOutcome, Runner};

/// Restart behaviour for a wrapped runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Never,
    OnFailure,
    Always,
}

pub struct RestartRunner<R: Runner> {
    inner: R,
    policy: Policy,
    backoff: Duration,
    /// Test seam: stop after this many runs even under Always. `None` = forever.
    max_runs: Option<u32>,
}

impl<R: Runner> RestartRunner<R> {
    pub fn new(inner: R, policy: Policy) -> Self {
        Self {
            inner,
            policy,
            backoff: Duration::from_millis(100),
            max_runs: None,
        }
    }

    /// Cap the number of runs (used by tests to bound `Always`).
    pub fn with_max_runs(mut self, max: u32) -> Self {
        self.max_runs = Some(max);
        self
    }

    fn should_restart(&self, outcome: RunOutcome) -> bool {
        match self.policy {
            Policy::Never => false,
            Policy::OnFailure => outcome == RunOutcome::Failure,
            Policy::Always => outcome != RunOutcome::Stopped,
        }
    }
}

#[async_trait]
impl<R: Runner> Runner for RestartRunner<R> {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn run(&mut self) -> crate::runner::Result<RunOutcome> {
        let mut runs = 0u32;
        loop {
            let outcome = self.inner.run().await?;
            runs += 1;
            if let Some(max) = self.max_runs {
                if runs >= max {
                    return Ok(outcome);
                }
            }
            if !self.should_restart(outcome) {
                return Ok(outcome);
            }
            info!(service = self.inner.id(), ?outcome, "restarting service");
            tokio::time::sleep(self.backoff).await;
        }
    }

    async fn stop(&mut self) -> crate::runner::Result<()> {
        self.inner.stop().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{Result as RunnerResult, RunnerError};

    /// A runner that returns a scripted sequence of outcomes.
    struct ScriptedRunner {
        id: String,
        outcomes: Vec<RunOutcome>,
        idx: usize,
    }

    #[async_trait]
    impl Runner for ScriptedRunner {
        fn id(&self) -> &str {
            &self.id
        }
        async fn run(&mut self) -> RunnerResult<RunOutcome> {
            let o = *self
                .outcomes
                .get(self.idx)
                .ok_or_else(|| RunnerError::Other("script exhausted".into()))?;
            self.idx += 1;
            Ok(o)
        }
        async fn stop(&mut self) -> RunnerResult<()> {
            Ok(())
        }
    }

    fn scripted(outcomes: Vec<RunOutcome>) -> ScriptedRunner {
        ScriptedRunner {
            id: "s".into(),
            outcomes,
            idx: 0,
        }
    }

    #[tokio::test]
    async fn never_does_not_restart() {
        let mut r = RestartRunner::new(scripted(vec![RunOutcome::Failure]), Policy::Never);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Failure);
    }

    #[tokio::test]
    async fn on_failure_restarts_until_success() {
        // Fail, fail, succeed → three runs, returns Success.
        let mut r = RestartRunner::new(
            scripted(vec![
                RunOutcome::Failure,
                RunOutcome::Failure,
                RunOutcome::Success,
            ]),
            Policy::OnFailure,
        );
        assert_eq!(r.run().await.unwrap(), RunOutcome::Success);
    }

    #[tokio::test]
    async fn always_restarts_but_respects_max_runs() {
        let mut r = RestartRunner::new(
            scripted(vec![RunOutcome::Success, RunOutcome::Success]),
            Policy::Always,
        )
        .with_max_runs(2);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Success);
    }

    #[tokio::test]
    async fn always_stops_on_stopped() {
        let mut r = RestartRunner::new(scripted(vec![RunOutcome::Stopped]), Policy::Always);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Stopped);
    }
}
