//! End-to-end boot harness: drives boot_sequence + shutdown_sequence over a
//! fake platform and a real process service, asserting the service is
//! supervised and reflected in the store.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{MachineConfig, MachineSection, Provider, RestartPolicy, ServiceConfig};
use machined_platform::{essential_mounts, FakePlatform};
use machined_resources::{Key, Resource, ResourceType, ServiceState};
use machined_runtime_core::Runtime;
use machined_sequencer::{boot_sequence, shutdown_sequence, SequencerCtx};
use machined_supervisor::ServiceManager;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn boots_supervises_and_shuts_down() {
    let platform = Arc::new(FakePlatform::new());
    let runtime = Runtime::new();
    let state = runtime.state();
    let shutdown = CancellationToken::new();
    let rt_token = shutdown.clone();
    let rt_handle = tokio::spawn(async move { runtime.run(rt_token).await });

    // A service that stays up long enough to observe Running.
    let cfg = MachineConfig {
        machine: MachineSection {
            hostname: Some("node-1".into()),
            sysctls: vec![],
            services: vec![ServiceConfig {
                id: "payload".into(),
                command: vec!["sleep".into(), "5".into()],
                depends_on: vec![],
                restart: RestartPolicy::Never,
            }],
            network: Default::default(),
        },
    };

    let services = Arc::new(Mutex::new(ServiceManager::new(state.clone())));
    let ctx = SequencerCtx {
        state: state.clone(),
        platform: platform.clone(),
        provider: Provider::new(cfg),
        services: services.clone(),
    };

    // Boot.
    boot_sequence().run(&ctx).await.expect("boot succeeds");

    // Essential mounts happened.
    assert_eq!(
        platform.recorded.lock().unwrap().mounts.len(),
        essential_mounts().len()
    );

    // The payload reaches Running.
    let key = Key::new("runtime", ResourceType::ServiceStatus, "payload");
    let mut running = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(obj) = state.get(&key) {
            if let Resource::ServiceStatus(s) = obj.spec {
                if s.state == ServiceState::Running {
                    running = true;
                    break;
                }
            }
        }
    }
    assert!(running, "payload service never reached Running");

    // Shutdown stops services cleanly.
    shutdown_sequence()
        .run(&ctx)
        .await
        .expect("shutdown succeeds");
    shutdown.cancel();
    let _ = rt_handle.await;
}
