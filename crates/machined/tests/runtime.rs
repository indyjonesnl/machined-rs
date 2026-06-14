//! End-to-end: RuntimeHealthController on the real Runtime against a fake CRI
//! client publishes a ready RuntimeStatus. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{MachineConfig, MachineSection, Provider};
use machined_controllers::runtime::{RuntimeHealthController, NS};
use machined_cri::FakeCriClient;
use machined_resources::{Key, Resource, ResourceType};
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn publishes_runtime_status() {
    let cri = Arc::new(
        FakeCriClient::new()
            .with_version("containerd", "2.0.0")
            .with_ready(true),
    );
    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: Default::default(),
            install: None,
            time: Default::default(),
            runtime: Default::default(),
            pods: vec![],
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(RuntimeHealthController::new(
        cri,
        Provider::new(config),
    )));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut ready = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(obj) = state.get(&Key::new(NS, ResourceType::RuntimeStatus, "containerd")) {
            if let Resource::RuntimeStatus(r) = obj.spec {
                if r.ready && r.version == "2.0.0" {
                    ready = true;
                    break;
                }
            }
        }
    }
    assert!(ready, "RuntimeStatus did not become ready");

    shutdown.cancel();
    let _ = handle.await;
}
