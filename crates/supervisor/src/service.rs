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

/// Sleep `dur`, but wake early if the stop intent is set — so a stop during a
/// long restart backoff is honoured promptly instead of holding stop_all for
/// the full delay. Mirrors the dep-gate select! used in run_supervised.
async fn backoff_sleep(
    stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    dur: std::time::Duration,
) {
    use std::sync::atomic::Ordering;
    tokio::select! {
        () = tokio::time::sleep(dur) => {}
        () = async {
            while !stop.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => {}
    }
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
    let mut backoff = Duration::from_secs(1);
    loop {
        if stop.load(Ordering::SeqCst) {
            publish_status(state, &id, ServiceState::Stopped, true, "stopped");
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
            publish_status(state, &id, ServiceState::Stopped, true, "stopped");
            return;
        }
        let started = std::time::Instant::now();
        let outcome = run_service(state, &mut runner).await;
        if stop.load(Ordering::SeqCst) {
            publish_status(state, &id, ServiceState::Stopped, true, "drained");
            return;
        }
        if !should_restart(policy, outcome) {
            return;
        }
        let (delay, next) = crate::restart::backoff_step(backoff, started.elapsed());
        backoff = next;
        info!(service = %id, ?outcome, ?delay, "restarting service after backoff");
        backoff_sleep(&stop, delay).await;
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

    // Always reports Failure, so a restart loop enters backoff every iteration.
    struct AlwaysFail(String);
    #[async_trait]
    impl Runner for AlwaysFail {
        fn id(&self) -> &str {
            &self.0
        }
        async fn run(&mut self) -> crate::runner::Result<RunOutcome> {
            Ok(RunOutcome::Failure)
        }
        async fn stop(&mut self) -> crate::runner::Result<()> {
            Ok(())
        }
    }

    // Runs in REAL time on purpose (NOT start_paused): the assertion is about
    // wall-clock stop-responsiveness, and under start_paused a `timeout` is itself
    // virtual — a non-stop-aware sleep would still complete in finite virtual time
    // and the test could not distinguish a hang. The first failure arms the base
    // 1s backoff; we request stop while parked in it and require the loop to return
    // in well under that second. A non-stop-aware `sleep(backoff)` would hold for
    // the full ~1s and blow the 400ms budget; the stop-aware backoff wakes at the
    // 50ms stop-poll cadence.
    #[tokio::test]
    async fn stop_during_backoff_returns_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let state = State::new();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, state2) = (stop.clone(), state.clone());
        let h = tokio::spawn(async move {
            run_supervised(
                &state2,
                AlwaysFail("loop".into()),
                Policy::Always,
                stop2,
                Arc::new(DefaultReadiness),
                &[],
            )
            .await;
        });
        // Let it fail once and enter the (1s) backoff sleep, then request stop.
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.store(true, Ordering::SeqCst);
        let t = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(3), h)
            .await
            .expect("must stop promptly")
            .unwrap();
        let elapsed = t.elapsed();
        assert!(
            elapsed < Duration::from_millis(400),
            "stop during backoff must wake the sleep, not wait it out (took {elapsed:?})"
        );
        let k = Key::new("runtime", ResourceType::ServiceStatus, "loop");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => assert_eq!(s.state, ServiceState::Stopped),
            _ => panic!(),
        }
    }

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
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
        // Wait past the restart backoff (>=1s) so the loop completes its backoff
        // sleep and re-gates: with the dep now not-ready it must park in Waiting.
        tokio::time::sleep(Duration::from_millis(1500)).await;
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
