//! Owns the set of services: starts them in dependency order (spawning each as
//! a background task) and stops them in reverse order on shutdown.

use std::collections::HashMap;
use std::sync::Arc;

use machined_config::{RestartPolicy, ServiceConfig};
use machined_runtime_core::State;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::process::ProcessRunner;
use crate::readiness::{wait_for_deps, ReadinessCheck};
use crate::restart::{Policy, RestartRunner};
use crate::service::run_service;

/// Translate the config restart policy into the supervisor policy.
fn policy_of(p: RestartPolicy) -> Policy {
    match p {
        RestartPolicy::Never => Policy::Never,
        RestartPolicy::OnFailure => Policy::OnFailure,
        RestartPolicy::Always => Policy::Always,
    }
}

/// Order services so dependencies start before dependents (Kahn's algorithm).
/// Returns the start order, or an error string on a missing dep / cycle.
pub fn start_order(services: &[ServiceConfig]) -> Result<Vec<String>, String> {
    let ids: Vec<String> = services.iter().map(|s| s.id.clone()).collect();
    let mut indegree: HashMap<&str, usize> = ids.iter().map(|i| (i.as_str(), 0)).collect();
    let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();

    for svc in services {
        for dep in &svc.depends_on {
            if !indegree.contains_key(dep.as_str()) {
                return Err(format!("service {} depends on unknown {}", svc.id, dep));
            }
            deps.entry(dep.as_str()).or_default().push(svc.id.as_str());
            *indegree.get_mut(svc.id.as_str()).unwrap() += 1;
        }
    }

    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&id, _)| id)
        .collect();
    queue.sort_unstable(); // deterministic order
    let mut order = Vec::new();

    while let Some(id) = queue.pop() {
        order.push(id.to_string());
        if let Some(children) = deps.get(id) {
            for &child in children {
                let e = indegree.get_mut(child).unwrap();
                *e -= 1;
                if *e == 0 {
                    queue.push(child);
                    queue.sort_unstable();
                }
            }
        }
    }

    if order.len() != services.len() {
        return Err("dependency cycle among services".into());
    }
    Ok(order)
}

/// Supervises the configured services over a shared store.
pub struct ServiceManager {
    state: State,
    handles: Vec<(String, JoinHandle<()>)>,
}

impl ServiceManager {
    pub fn new(state: State) -> Self {
        Self {
            state,
            handles: Vec::new(),
        }
    }

    /// Start every service as a background task, in dependency order. Each
    /// task first waits (publishing Waiting) until `check` reports all of its
    /// depends_on ready, then runs the service.
    pub fn start_all(
        &mut self,
        services: &[ServiceConfig],
        check: Arc<dyn ReadinessCheck>,
    ) -> Result<(), String> {
        let order = start_order(services)?;
        let by_id: HashMap<&str, &ServiceConfig> =
            services.iter().map(|s| (s.id.as_str(), s)).collect();

        for id in order {
            let cfg = by_id[id.as_str()];
            let state = self.state.clone();
            let deps = cfg.depends_on.clone();
            let check = check.clone();
            let sid = cfg.id.clone();
            let runner = RestartRunner::new(
                ProcessRunner::new(cfg.id.clone(), cfg.command.clone()),
                policy_of(cfg.restart),
            );
            info!(service = %cfg.id, "starting service");
            let handle = tokio::spawn(async move {
                wait_for_deps(&state, check.as_ref(), &sid, &deps).await;
                run_service(&state, runner).await;
            });
            self.handles.push((cfg.id.clone(), handle));
        }
        Ok(())
    }

    /// Stop all services in reverse start order, aborting their tasks. The
    /// aborted task drops its `ProcessRunner`, whose `kill_on_drop` reaps the
    /// child process (graceful SIGTERM + grace timeout lands in M5).
    pub async fn stop_all(&mut self) {
        while let Some((id, handle)) = self.handles.pop() {
            info!(service = %id, "stopping service");
            handle.abort();
            if let Err(e) = handle.await {
                if !e.is_cancelled() {
                    warn!(service = %id, "join error on stop: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::RestartPolicy;

    fn svc(id: &str, deps: &[&str]) -> ServiceConfig {
        ServiceConfig {
            id: id.into(),
            command: vec!["true".into()],
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            restart: RestartPolicy::Never,
        }
    }

    #[test]
    fn orders_dependencies_first() {
        let services = vec![svc("payload", &["containerd"]), svc("containerd", &[])];
        let order = start_order(&services).unwrap();
        let ci = order.iter().position(|s| s == "containerd").unwrap();
        let pi = order.iter().position(|s| s == "payload").unwrap();
        assert!(ci < pi, "containerd must start before payload: {order:?}");
    }

    #[test]
    fn unknown_dependency_errors() {
        let services = vec![svc("payload", &["ghost"])];
        assert!(start_order(&services).is_err());
    }

    #[test]
    fn cycle_errors() {
        let services = vec![svc("a", &["b"]), svc("b", &["a"])];
        assert!(start_order(&services).is_err());
    }

    #[tokio::test]
    async fn start_all_publishes_status() {
        let state = State::new();
        let mut mgr = ServiceManager::new(state.clone());
        // A short-lived service so the task finishes quickly.
        mgr.start_all(
            &[ServiceConfig {
                id: "blip".into(),
                command: vec!["true".into()],
                depends_on: vec![],
                restart: RestartPolicy::Never,
            }],
            Arc::new(crate::readiness::DefaultReadiness),
        )
        .unwrap();

        // Give the task time to publish + run.
        let k = machined_resources::Key::new(
            "runtime",
            machined_resources::ResourceType::ServiceStatus,
            "blip",
        );
        let mut seen = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if state.get(&k).is_ok() {
                seen = true;
                break;
            }
        }
        assert!(seen, "ServiceStatus was never published");
        mgr.stop_all().await;
    }
}
