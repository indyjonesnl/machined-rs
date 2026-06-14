//! End-to-end: the TimeSyncController on the real Runtime against a fake
//! TimeSync syncs and publishes TimeStatus. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{MachineConfig, MachineSection, Provider, TimeSection};
use machined_controllers::time::{TimeSyncController, NS};
use machined_resources::{Resource, ResourceType};
use machined_runtime_core::Runtime;
use machined_time::FakeTimeSync;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn syncs_time_and_publishes_status() {
    let sync = Arc::new(FakeTimeSync::new().with_offset("a:123", 300_000_000)); // 300ms
    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: Default::default(),
            install: None,
            time: TimeSection {
                servers: vec!["a".into()],
                disabled: false,
            },
            runtime: Default::default(),
            pods: vec![],
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(TimeSyncController::new(
        sync.clone(),
        Provider::new(config),
    )));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut synced = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(obj) = state.get(&machined_resources::Key::new(
            NS,
            ResourceType::TimeStatus,
            "time",
        )) {
            if let Resource::TimeStatus(t) = obj.spec {
                if t.synced {
                    synced = true;
                    break;
                }
            }
        }
    }
    assert!(synced, "time did not sync");
    assert_eq!(sync.steps(), vec![300_000_000]);

    shutdown.cancel();
    let _ = handle.await;
}
