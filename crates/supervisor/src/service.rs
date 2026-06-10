//! Drives one service: runs its `Runner` and reflects lifecycle transitions
//! into the shared `State` as a `ServiceStatus` resource.

use machined_resources::{
    Key, Resource, ResourceObject, ResourceType, ServiceState, ServiceStatusSpec,
};
use machined_runtime_core::State;
use tracing::{debug, info};

use crate::runner::{RunOutcome, Runner};

const NS: &str = "runtime";

fn key(id: &str) -> Key {
    Key::new(NS, ResourceType::ServiceStatus, id)
}

/// Write/refresh the ServiceStatus resource for `id` in the store.
pub fn publish_status(state: &State, id: &str, st: ServiceState, healthy: bool, message: &str) {
    let spec = ServiceStatusSpec {
        service_id: id.to_string(),
        state: st,
        healthy,
        last_message: message.to_string(),
    };
    let k = key(id);
    match state.get(&k) {
        Ok(existing) => {
            // Single-writer-per-service (one run_service task owns each id), so a
            // conflict here is unexpected; log it rather than silently dropping
            // the transition.
            if let Err(e) =
                state.update(&k, existing.metadata.version, Resource::ServiceStatus(spec))
            {
                debug!(service = id, error = %e, "dropped ServiceStatus update");
            }
        }
        Err(_) => {
            let _ = state.create(ResourceObject::new(NS, id, Resource::ServiceStatus(spec)));
        }
    }
}

/// Drive `runner` to completion, publishing status transitions to `state`.
pub async fn run_service<R: Runner>(state: &State, mut runner: R) -> RunOutcome {
    let id = runner.id().to_string();
    publish_status(state, &id, ServiceState::Preparing, false, "starting");
    publish_status(state, &id, ServiceState::Running, true, "running");
    info!(service = %id, "service running");

    let outcome = match runner.run().await {
        Ok(o) => o,
        Err(e) => {
            publish_status(state, &id, ServiceState::Failed, false, &e.to_string());
            return RunOutcome::Failure;
        }
    };

    let (final_state, msg) = match outcome {
        RunOutcome::Success => (ServiceState::Finished, "exited 0"),
        RunOutcome::Failure => (ServiceState::Failed, "exited non-zero"),
        RunOutcome::Stopped => (ServiceState::Finished, "stopped"),
    };
    publish_status(state, &id, final_state, outcome != RunOutcome::Failure, msg);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Runner;
    use async_trait::async_trait;

    struct Instant(RunOutcome, String);

    #[async_trait]
    impl Runner for Instant {
        fn id(&self) -> &str {
            &self.1
        }
        async fn run(&mut self) -> crate::runner::Result<RunOutcome> {
            Ok(self.0)
        }
        async fn stop(&mut self) -> crate::runner::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn publishes_running_then_finished() {
        let state = State::new();
        let outcome = run_service(&state, Instant(RunOutcome::Success, "svc".into())).await;
        assert_eq!(outcome, RunOutcome::Success);

        let obj = state.get(&key("svc")).unwrap();
        match obj.spec {
            Resource::ServiceStatus(s) => {
                assert_eq!(s.state, ServiceState::Finished);
            }
            _ => panic!("wrong type"),
        }
    }

    #[tokio::test]
    async fn failure_marks_failed_unhealthy() {
        let state = State::new();
        run_service(&state, Instant(RunOutcome::Failure, "svc".into())).await;
        let obj = state.get(&key("svc")).unwrap();
        match obj.spec {
            Resource::ServiceStatus(s) => {
                assert_eq!(s.state, ServiceState::Failed);
                assert!(!s.healthy);
            }
            _ => panic!("wrong type"),
        }
    }
}
