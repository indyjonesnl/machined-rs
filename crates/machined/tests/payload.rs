//! M4 acceptance: a payload service with depends_on [containerd] stays Waiting
//! until the CRI probe reports the runtime ready, then starts. Hermetic: the
//! "containerd" stand-in is `sleep 30`; CRI is a fake that flips.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{RestartPolicy, ServiceConfig};
use machined_cri::{CriClient, CriError, RuntimeVersion};
use machined_resources::{Key, Resource, ResourceType, RuntimeStatus, ServiceState};
use machined_runtime_core::State;
use machined_supervisor::{DefaultReadiness, ReadinessCheck, ServiceManager};

/// The machined RuntimeReadiness rule, restated for the test (the binary's
/// private type isn't linkable from an integration test).
struct RuntimeReadiness;
impl ReadinessCheck for RuntimeReadiness {
    fn is_ready(&self, state: &State, dep_id: &str) -> bool {
        let base = DefaultReadiness.is_ready(state, dep_id);
        if dep_id != "containerd" {
            return base;
        }
        let cri = matches!(
            state
                .get(&Key::new("runtime", ResourceType::RuntimeStatus, "containerd"))
                .map(|o| o.spec),
            Ok(Resource::RuntimeStatus(r)) if r.ready
        );
        base && cri
    }
}

/// A flippable CRI fake (FakeCriClient's ready is fixed at construction).
struct FlipCri {
    ready: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl CriClient for FlipCri {
    async fn version(&self) -> Result<RuntimeVersion, CriError> {
        Ok(RuntimeVersion {
            runtime_name: "containerd".into(),
            runtime_version: "2.0.0".into(),
        })
    }
    async fn ready(&self) -> Result<bool, CriError> {
        Ok(self.ready.load(std::sync::atomic::Ordering::SeqCst))
    }
}

fn svc_state(state: &State, id: &str) -> Option<ServiceState> {
    match state
        .get(&Key::new("runtime", ResourceType::ServiceStatus, id))
        .ok()?
        .spec
    {
        Resource::ServiceStatus(s) => Some(s.state),
        _ => None,
    }
}

async fn wait_for(state: &State, id: &str, want: ServiceState, budget_ms: u64) -> bool {
    for _ in 0..(budget_ms / 20) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if svc_state(state, id) == Some(want) {
            return true;
        }
    }
    false
}

/// Wait until the service reaches ANY of `want` (one budget, no dead-wait when
/// a short-lived command races past a single state).
async fn wait_for_any(state: &State, id: &str, want: &[ServiceState], budget_ms: u64) -> bool {
    for _ in 0..(budget_ms / 20) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Some(s) = svc_state(state, id) {
            if want.contains(&s) {
                return true;
            }
        }
    }
    false
}

#[tokio::test]
async fn payload_waits_for_cri_then_starts() {
    let state = State::new();
    let cri = Arc::new(FlipCri {
        ready: std::sync::atomic::AtomicBool::new(false),
    });

    // Publish RuntimeStatus the way RuntimeHealthController would (driving the
    // controller's 10s timer in-test is slow; the rule consumes the resource,
    // so publishing it directly keeps the test fast and equivalent).
    let publish_runtime_status = |state: &State, ready: bool| {
        let obj = machined_resources::ResourceObject::new(
            "runtime",
            "containerd",
            Resource::RuntimeStatus(RuntimeStatus {
                ready,
                name: "containerd".into(),
                version: "2.0.0".into(),
            }),
        );
        let k = Key::new("runtime", ResourceType::RuntimeStatus, "containerd");
        match state.get(&k) {
            Ok(cur) => {
                let _ = state.update(&k, cur.metadata.version, obj.spec);
            }
            Err(_) => {
                let _ = state.create(obj);
            }
        }
    };
    publish_runtime_status(&state, false);

    let services = vec![
        // Hermetic stand-in for containerd: long-running, harmless.
        ServiceConfig {
            id: "containerd".into(),
            command: vec!["sleep".into(), "30".into()],
            depends_on: vec![],
            restart: RestartPolicy::Never,
        },
        ServiceConfig {
            id: "payload".into(),
            command: vec!["true".into()],
            depends_on: vec!["containerd".into()],
            restart: RestartPolicy::Never,
        },
    ];

    let mut mgr = ServiceManager::new(state.clone());
    mgr.start_all(&services, Arc::new(RuntimeReadiness))
        .unwrap();

    // containerd (the stand-in) runs…
    assert!(
        wait_for(&state, "containerd", ServiceState::Running, 3000).await,
        "stand-in containerd should run"
    );
    // …but the payload stays Waiting: CRI not ready.
    assert!(
        wait_for(&state, "payload", ServiceState::Waiting, 3000).await,
        "payload should be Waiting while CRI is not ready"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        svc_state(&state, "payload"),
        Some(ServiceState::Waiting),
        "payload must NOT start before CRI is ready"
    );

    // Flip CRI ready (as the health controller would observe + publish).
    cri.ready.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(cri.ready().await.unwrap());
    publish_runtime_status(&state, true);

    // The payload now starts and finishes (command `true` → Finished).
    let started = wait_for_any(
        &state,
        "payload",
        &[ServiceState::Running, ServiceState::Finished],
        5000,
    )
    .await;
    assert!(started, "payload must start once CRI is ready");

    mgr.stop_all().await;
}

#[tokio::test]
async fn stop_all_while_waiting_is_clean() {
    // A service stuck Waiting (dep never ready) is aborted cleanly by stop_all.
    let state = State::new();
    let services = vec![ServiceConfig {
        id: "stuck".into(),
        command: vec!["true".into()],
        depends_on: vec!["never-ready".into()],
        restart: RestartPolicy::Never,
    }];
    // The dep must be DETERMINISTICALLY never-ready. A fast-failing command
    // (e.g. `false`) briefly publishes Running+healthy before exiting, and the
    // dependent's readiness check can race into that window (pre-existing M1
    // semantics; per-service probes are a later milestone). A binary that fails
    // to spawn at all never reaches Running.
    let services = {
        let mut s = services;
        s.insert(
            0,
            ServiceConfig {
                id: "never-ready".into(),
                command: vec!["/nonexistent-machined-test-binary".into()],
                depends_on: vec![],
                restart: RestartPolicy::Never,
            },
        );
        s
    };

    let mut mgr = ServiceManager::new(state.clone());
    mgr.start_all(&services, Arc::new(DefaultReadiness))
        .unwrap();

    // The dependent parks in Waiting.
    assert!(
        wait_for(&state, "stuck", ServiceState::Waiting, 3000).await,
        "dependent should be Waiting on the failed dep"
    );

    // stop_all must return promptly (aborting the parked task) — bound it.
    tokio::time::timeout(Duration::from_secs(5), mgr.stop_all())
        .await
        .expect("stop_all must not hang on a Waiting service");
}
