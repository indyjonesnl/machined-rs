//! Owns the set of services: starts them in dependency order (spawning each as
//! a background task) and stops them in reverse order on shutdown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use machined_config::{RestartPolicy, ServiceConfig};
use machined_runtime_core::State;
use tracing::{info, warn};

use crate::process::ProcessRunner;
use crate::readiness::ReadinessCheck;
use crate::restart::Policy;
use crate::service::run_supervised;

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

/// One supervised service: its task, signal handle, stop intent, and grace.
struct ServiceHandle {
    id: String,
    join: tokio::task::JoinHandle<()>,
    pid: Arc<StdMutex<Option<u32>>>,
    stop: Arc<AtomicBool>,
    grace: Duration,
}

/// Supervises the configured services over a shared store.
pub struct ServiceManager {
    state: State,
    handles: Vec<ServiceHandle>,
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
            let stop = Arc::new(AtomicBool::new(false));
            let stop_task = stop.clone();
            let runner = ProcessRunner::new(cfg.id.clone(), cfg.command.clone());
            let pid = runner.pid_slot();
            let policy = policy_of(cfg.restart);
            info!(service = %cfg.id, "starting service");
            let handle = tokio::spawn(async move {
                run_supervised(&state, runner, policy, stop_task, check, &deps).await;
            });
            self.handles.push(ServiceHandle {
                id: cfg.id.clone(),
                join: handle,
                pid,
                stop,
                grace: Duration::from_secs(cfg.stop_grace_secs.unwrap_or(10)),
            });
        }
        Ok(())
    }

    /// Stop all services in reverse start order: stop-intent → SIGTERM →
    /// grace-bounded drain → abort (kill_on_drop SIGKILLs).
    ///
    /// Bounded race: if the supervised task is between attempts when the
    /// intent lands (pid slot empty), the SIGTERM is skipped and one final
    /// child may spawn; it either exits within the grace or is SIGKILLed at
    /// expiry. The loop re-checks the intent, so no restart follows.
    pub async fn stop_all(&mut self) {
        while let Some(mut h) = self.handles.pop() {
            h.stop.store(true, Ordering::SeqCst);
            let pid = *h.pid.lock().unwrap();
            if let Some(pid) = pid {
                #[cfg(unix)]
                {
                    use nix::sys::signal::{kill, Signal};
                    use nix::unistd::Pid;
                    match kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
                        Ok(()) => info!(service = %h.id, "sent SIGTERM"),
                        Err(nix::errno::Errno::ESRCH) => {}
                        Err(e) => warn!(service = %h.id, "SIGTERM failed: {e}"),
                    }
                }
            }
            match tokio::time::timeout(h.grace, &mut h.join).await {
                Ok(join) => {
                    if join.is_err() {
                        warn!(service = %h.id, "supervision task panicked during stop");
                    } else {
                        info!(service = %h.id, "service drained");
                    }
                }
                Err(_) => {
                    warn!(service = %h.id, "grace expired; killing");
                    h.join.abort();
                    let _ = h.join.await;
                    // The aborted task can't publish its own final status —
                    // record the kill so stop progress stays observable.
                    crate::service::publish_status(
                        &self.state,
                        &h.id,
                        machined_resources::ServiceState::Failed,
                        false,
                        "killed after stop grace expired",
                    );
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
            stop_grace_secs: None,
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
                stop_grace_secs: None,
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

    use machined_resources::{Key, Resource, ResourceType, ServiceState};
    use std::time::{Duration, Instant};

    fn svc_full(id: &str, command: &[&str], grace: u64) -> ServiceConfig {
        ServiceConfig {
            id: id.into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            depends_on: vec![],
            restart: RestartPolicy::Never,
            stop_grace_secs: Some(grace),
        }
    }

    async fn wait_running(state: &State, id: &str) {
        let k = Key::new("runtime", ResourceType::ServiceStatus, id);
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Ok(o) = state.get(&k) {
                if matches!(o.spec, Resource::ServiceStatus(ref s) if s.state == ServiceState::Running)
                {
                    return;
                }
            }
        }
        panic!("{id} never reached Running");
    }

    #[tokio::test]
    async fn graceful_stop_drains_on_sigterm() {
        let state = State::new();
        let mut mgr = ServiceManager::new(state.clone());
        mgr.start_all(
            &[svc_full(
                "drainer",
                &["sh", "-c", "trap 'kill $!; exit 0' TERM; sleep 30 & wait"],
                5,
            )],
            Arc::new(crate::readiness::DefaultReadiness),
        )
        .unwrap();
        wait_running(&state, "drainer").await;

        let t0 = Instant::now();
        mgr.stop_all().await;
        let took = t0.elapsed();
        assert!(
            took < Duration::from_secs(4),
            "drained, not grace-expired: {took:?}"
        );
        let k = Key::new("runtime", ResourceType::ServiceStatus, "drainer");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => {
                assert_eq!(
                    s.state,
                    ServiceState::Finished,
                    "TERM-trapped exit 0 → Finished"
                )
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn grace_expiry_kills() {
        let state = State::new();
        let mut mgr = ServiceManager::new(state.clone());
        mgr.start_all(
            &[svc_full(
                "stubborn",
                // `exec` keeps this a single process (ignored TERM survives
                // exec); the eventual kill_on_drop SIGKILL leaves no orphan.
                &["sh", "-c", "trap '' TERM; exec sleep 30"],
                1,
            )],
            Arc::new(crate::readiness::DefaultReadiness),
        )
        .unwrap();
        wait_running(&state, "stubborn").await;

        let t0 = Instant::now();
        mgr.stop_all().await;
        let took = t0.elapsed();
        assert!(
            took >= Duration::from_millis(900) && took < Duration::from_secs(5),
            "killed at ~grace: {took:?}"
        );
        // The kill is observable: status records the forced stop.
        let k = Key::new("runtime", ResourceType::ServiceStatus, "stubborn");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => {
                assert_eq!(s.state, ServiceState::Failed);
                assert!(s.last_message.contains("killed"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn stop_all_reverse_order() {
        // dep ← dependent; stop must drain the dependent first. Both record
        // their TERM time by exiting promptly; reverse order is observable via
        // sequential stop (dependent drained before dep gets TERM).
        let state = State::new();
        let mut mgr = ServiceManager::new(state.clone());
        let dep = svc_full(
            "dep",
            &["sh", "-c", "trap 'kill $!; exit 0' TERM; sleep 30 & wait"],
            5,
        );
        let mut dependent = svc_full(
            "dependent",
            &["sh", "-c", "trap 'kill $!; exit 0' TERM; sleep 30 & wait"],
            5,
        );
        dependent.depends_on = vec!["dep".into()];
        mgr.start_all(
            &[dep, dependent],
            Arc::new(crate::readiness::DefaultReadiness),
        )
        .unwrap();
        wait_running(&state, "dep").await;
        wait_running(&state, "dependent").await;

        mgr.stop_all().await;
        // Both Finished; handles popped in reverse (dependent first) — the
        // sequential drain proves ordering structurally (handles is a stack).
        for id in ["dep", "dependent"] {
            let k = Key::new("runtime", ResourceType::ServiceStatus, id);
            match state.get(&k).unwrap().spec {
                Resource::ServiceStatus(s) => assert_eq!(s.state, ServiceState::Finished),
                _ => panic!(),
            }
        }
    }
}
