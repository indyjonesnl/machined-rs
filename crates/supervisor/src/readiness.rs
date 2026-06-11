//! Health-gated dependency readiness: when may a dependent service start?

use std::time::Duration;

use machined_resources::{Key, Resource, ResourceType, ServiceState};
use machined_runtime_core::State;
use tracing::warn;

use crate::service::publish_status;

/// Decides when a dependency service is ready to be depended on.
pub trait ReadinessCheck: Send + Sync {
    /// True iff `dep_id` is ready (its dependents may start).
    fn is_ready(&self, state: &State, dep_id: &str) -> bool;
}

/// Default rule: the dep's ServiceStatus is (Running && healthy) OR Finished —
/// a run-once dependency that completed successfully is satisfied. Anything
/// else (absent, Waiting, Preparing, Failed, Skipped, unhealthy) is not ready.
pub struct DefaultReadiness;

impl ReadinessCheck for DefaultReadiness {
    fn is_ready(&self, state: &State, dep_id: &str) -> bool {
        let key = Key::new("runtime", ResourceType::ServiceStatus, dep_id);
        match state.get(&key).map(|o| o.spec) {
            Ok(Resource::ServiceStatus(s)) => {
                (s.state == ServiceState::Running && s.healthy) || s.state == ServiceState::Finished
            }
            _ => false,
        }
    }
}

/// Block until every dep is ready, publishing a Waiting status meanwhile.
/// Returns immediately when `deps` is empty or already ready.
pub(crate) async fn wait_for_deps(
    state: &State,
    check: &dyn ReadinessCheck,
    id: &str,
    deps: &[String],
) {
    let ready = |deps: &[String]| deps.iter().all(|d| check.is_ready(state, d));
    if deps.is_empty() || ready(deps) {
        return;
    }
    publish_status(
        state,
        id,
        ServiceState::Waiting,
        false,
        &format!("waiting for: {}", deps.join(",")),
    );
    let mut ticks: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if ready(deps) {
            return;
        }
        ticks += 1;
        if ticks.is_multiple_of(150) {
            let pending: Vec<&String> = deps.iter().filter(|d| !check.is_ready(state, d)).collect();
            warn!(service = id, pending = ?pending, "still waiting for dependencies");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{ResourceObject, ServiceStatusSpec};

    fn put(state: &State, id: &str, st: ServiceState, healthy: bool) {
        let _ = state.create(ResourceObject::new(
            "runtime",
            id,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: id.into(),
                state: st,
                healthy,
                last_message: String::new(),
            }),
        ));
    }

    #[test]
    fn default_readiness_truth_table() {
        let state = State::new();
        let r = DefaultReadiness;
        assert!(!r.is_ready(&state, "absent"));

        put(&state, "running-healthy", ServiceState::Running, true);
        put(&state, "running-unhealthy", ServiceState::Running, false);
        put(&state, "finished", ServiceState::Finished, false);
        put(&state, "failed", ServiceState::Failed, false);
        put(&state, "waiting", ServiceState::Waiting, false);
        put(&state, "preparing", ServiceState::Preparing, false);
        put(&state, "skipped", ServiceState::Skipped, false);

        assert!(r.is_ready(&state, "running-healthy"));
        assert!(!r.is_ready(&state, "running-unhealthy"));
        assert!(r.is_ready(&state, "finished"), "run-once success satisfies");
        assert!(!r.is_ready(&state, "failed"));
        assert!(!r.is_ready(&state, "waiting"));
        assert!(!r.is_ready(&state, "preparing"));
        assert!(!r.is_ready(&state, "skipped"));
    }

    #[tokio::test]
    async fn wait_unblocks_when_dep_flips() {
        let state = State::new();
        put(&state, "dep", ServiceState::Preparing, false);

        let s2 = state.clone();
        let waiter = tokio::spawn(async move {
            wait_for_deps(&s2, &DefaultReadiness, "svc", &["dep".to_string()]).await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiter.is_finished(), "must still be waiting");

        // svc shows Waiting in the store.
        let k = Key::new("runtime", ResourceType::ServiceStatus, "svc");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => assert_eq!(s.state, ServiceState::Waiting),
            _ => panic!("wrong type"),
        }

        // Flip the dep → the waiter finishes.
        let k = Key::new("runtime", ResourceType::ServiceStatus, "dep");
        let cur = state.get(&k).unwrap();
        state
            .update(
                &k,
                cur.metadata.version,
                Resource::ServiceStatus(ServiceStatusSpec {
                    service_id: "dep".into(),
                    state: ServiceState::Running,
                    healthy: true,
                    last_message: String::new(),
                }),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), waiter)
            .await
            .expect("waiter must unblock")
            .unwrap();
    }
}
