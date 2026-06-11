//! End-to-end: discovery + provisioner controllers on the real Runtime against a
//! shared fake backend. Proves the discovery barrier — the provisioner waits for
//! discovery to publish DiskStatus before provisioning the (wipe:true) install
//! disk and publishing VolumeStatus. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_block::{DiskInfo, FakeBlockBackend};
use machined_config::{InstallSection, MachineConfig, MachineSection, Provider};
use machined_controllers::block::{DiskDiscoveryController, VolumeProvisionerController, NS};
use machined_resources::ResourceType;
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn provisions_install_disk_after_discovery_barrier() {
    let backend = Arc::new(FakeBlockBackend::new().with_disk(DiskInfo {
        name: "vda".into(),
        path: "/dev/vda".into(),
        size_bytes: 16 << 30,
        model: "VIRT".into(),
        serial: "V1".into(),
        rotational: false,
        read_only: false,
    }));

    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: Default::default(),
            install: Some(InstallSection {
                disk: "/dev/vda".into(),
                // Blank-looking disk requires explicit wipe to provision.
                wipe: true,
            }),
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    // Both controllers share the fake backend. Discovery publishes DiskStatus
    // (the barrier); the provisioner waits for it before acting.
    runtime.register(Box::new(DiskDiscoveryController::new(backend.clone())));
    runtime.register(Box::new(VolumeProvisionerController::new(
        backend.clone(),
        Provider::new(config),
    )));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut ok = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if state.list(NS, ResourceType::VolumeStatus).len() == 3 {
            ok = true;
            break;
        }
    }
    assert!(
        ok,
        "provisioner did not publish 3 VolumeStatus after discovery"
    );
    assert_eq!(backend.creates().len(), 1);
    assert_eq!(backend.formats().len(), 3);

    shutdown.cancel();
    let _ = handle.await;
}
