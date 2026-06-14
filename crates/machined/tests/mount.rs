//! End-to-end: the full block pipeline on the real Runtime against fakes —
//! discovery → provision (wipe:true) → mount. Asserts the provisioned volumes
//! are mounted at their fixed mountpoints. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_block::{DiskInfo, FakeBlockBackend};
use machined_config::{InstallSection, MachineConfig, MachineSection, Provider};
use machined_controllers::block::{
    DiskDiscoveryController, VolumeMountController, VolumeProvisionerController, NS,
};
use machined_platform::FakePlatform;
use machined_resources::ResourceType;
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn pipeline_discovers_provisions_and_mounts() {
    let block = Arc::new(FakeBlockBackend::new().with_disk(DiskInfo {
        name: "vda".into(),
        path: "/dev/vda".into(),
        size_bytes: 16 << 30,
        model: "VIRT".into(),
        serial: "V1".into(),
        rotational: false,
        read_only: false,
    }));
    let platform = Arc::new(FakePlatform::new());

    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: Default::default(),
            install: Some(InstallSection {
                disk: "/dev/vda".into(),
                wipe: true,
            }),
            time: Default::default(),
            runtime: Default::default(),
            pods: vec![],
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(DiskDiscoveryController::new(block.clone())));
    runtime.register(Box::new(VolumeProvisionerController::new(
        block.clone(),
        Provider::new(config),
    )));
    runtime.register(Box::new(VolumeMountController::new(platform.clone())));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut ok = false;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if state.list(NS, ResourceType::MountStatus).len() == 3 {
            ok = true;
            break;
        }
    }
    assert!(ok, "pipeline did not mount the 3 provisioned volumes");

    // The three system volumes are mounted at their fixed targets.
    let targets: Vec<String> = platform
        .recorded
        .lock()
        .unwrap()
        .mounts
        .iter()
        .map(|m| m.target.clone())
        .collect();
    assert!(targets.contains(&"/boot".to_string()));
    assert!(targets.contains(&"/system/state".to_string()));
    assert!(targets.contains(&"/var".to_string()));

    shutdown.cancel();
    let _ = handle.await;
}
