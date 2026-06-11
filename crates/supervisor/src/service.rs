//! Drives one service: runs its `Runner` and reflects lifecycle transitions
//! into the shared `State` as a `ServiceStatus` resource.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use machined_resources::{
    Key, Resource, ResourceObject, ResourceType, ServiceState, ServiceStatusSpec,
};
use machined_runtime_core::State;
use tracing::{debug, info};

use crate::readiness::{wait_for_deps, ReadinessCheck};
use crate::restart::{should_restart, Policy};
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

/// Supervise one service: gate on deps, run, apply the restart policy — until
/// the policy says stop or a stop intent is set. Each attempt re-gates and
/// re-publishes per-attempt status (Waiting → Preparing → Running → …).
pub async fn run_supervised<R: Runner>(
    state: &State,
    mut runner: R,
    policy: Policy,
    stop: Arc<AtomicBool>,
    check: Arc<dyn ReadinessCheck>,
    deps: &[String],
) {
    let id = runner.id().to_string();
    let backoff = Duration::from_millis(100);
    loop {
        if stop.load(Ordering::SeqCst) {
            publish_status(state, &id, ServiceState::Finished, true, "stopped");
            return;
        }
        // Gate on deps, but stay responsive to the stop intent while parked
        // (otherwise a Waiting service would hold stop_all for its full grace).
        tokio::select! {
            () = wait_for_deps(state, check.as_ref(), &id, deps) => {}
            () = async {
                while !stop.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            } => {}
        }
        if stop.load(Ordering::SeqCst) {
            publish_status(state, &id, ServiceState::Finished, true, "stopped");
            return;
        }
        let outcome = run_service(state, &mut runner).await;
        if stop.load(Ordering::SeqCst) || !should_restart(policy, outcome) {
            return;
        }
        info!(service = %id, ?outcome, "restarting service");
        tokio::time::sleep(backoff).await;
    }
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

    use crate::readiness::DefaultReadiness;
    use crate::restart::Policy;

    struct Scripted {
        id: String,
        outcomes: Vec<RunOutcome>,
        idx: usize,
    }

    #[async_trait]
    impl Runner for Scripted {
        fn id(&self) -> &str {
            &self.id
        }
        async fn run(&mut self) -> crate::runner::Result<RunOutcome> {
            let o = self
                .outcomes
                .get(self.idx)
                .copied()
                .unwrap_or(RunOutcome::Stopped);
            self.idx += 1;
            Ok(o)
        }
        async fn stop(&mut self) -> crate::runner::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn supervised_on_failure_restarts_until_success() {
        let state = State::new();
        let r = Scripted {
            id: "s".into(),
            outcomes: vec![
                RunOutcome::Failure,
                RunOutcome::Failure,
                RunOutcome::Success,
            ],
            idx: 0,
        };
        run_supervised(
            &state,
            r,
            Policy::OnFailure,
            Arc::new(AtomicBool::new(false)),
            Arc::new(DefaultReadiness),
            &[],
        )
        .await;
        // Final status: Finished (the last run succeeded).
        let k = Key::new("runtime", ResourceType::ServiceStatus, "s");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => assert_eq!(s.state, ServiceState::Finished),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn supervised_stop_intent_prevents_restart() {
        let state = State::new();
        let stop = Arc::new(AtomicBool::new(false));
        let r = Scripted {
            id: "s".into(),
            outcomes: vec![RunOutcome::Success; 100],
            idx: 0,
        };
        let s2 = state.clone();
        let st2 = stop.clone();
        let h = tokio::spawn(async move {
            run_supervised(&s2, r, Policy::Always, st2, Arc::new(DefaultReadiness), &[]).await;
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        stop.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), h)
            .await
            .expect("must stop restarting")
            .unwrap();
    }

    #[tokio::test]
    async fn supervised_restart_re_gates_on_deps() {
        // dep ready → first run happens; dep flips not-ready → the restart parks
        // in Waiting instead of re-running.
        let state = State::new();
        let dep_key = Key::new("runtime", ResourceType::ServiceStatus, "dep");
        let _ = state.create(machined_resources::ResourceObject::new(
            "runtime",
            "dep",
            Resource::ServiceStatus(machined_resources::ServiceStatusSpec {
                service_id: "dep".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        ));
        let r = Scripted {
            id: "s".into(),
            outcomes: vec![RunOutcome::Failure; 100],
            idx: 0,
        };
        let s2 = state.clone();
        let h = tokio::spawn(async move {
            run_supervised(
                &s2,
                r,
                Policy::OnFailure,
                Arc::new(AtomicBool::new(false)),
                Arc::new(DefaultReadiness),
                &["dep".to_string()],
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        // Flip the dep to Failed → not ready; the next restart must park.
        let cur = state.get(&dep_key).unwrap();
        state
            .update(
                &dep_key,
                cur.metadata.version,
                Resource::ServiceStatus(machined_resources::ServiceStatusSpec {
                    service_id: "dep".into(),
                    state: ServiceState::Failed,
                    healthy: false,
                    last_message: String::new(),
                }),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let k = Key::new("runtime", ResourceType::ServiceStatus, "s");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => assert_eq!(
                s.state,
                ServiceState::Waiting,
                "restart must re-gate on deps"
            ),
            _ => panic!(),
        }
        h.abort();
        let _ = h.await;
    }
}
