//! End-to-end: the discovery controller on the real Runtime against a fake
//! block backend populates the store with disk + volume status. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_block::{DiskInfo, FakeBlockBackend, FsType, VolumeInfo};
use machined_controllers::block::{DiskDiscoveryController, NS};
use machined_resources::{Resource, ResourceType};
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn discovery_populates_store() {
    let backend = Arc::new(
        FakeBlockBackend::new()
            .with_disk(DiskInfo {
                name: "vda".into(),
                path: "/dev/vda".into(),
                size_bytes: 8 << 30,
                model: "VIRT".into(),
                serial: "V1".into(),
                rotational: false,
                read_only: false,
            })
            .with_volume(VolumeInfo {
                device: "/dev/vda1".into(),
                disk: "vda".into(),
                partition_uuid: "u".into(),
                partition_label: "STATE".into(),
                partition_type_guid: "g".into(),
                fs_type: Some(FsType::Ext4),
                fs_label: None,
                fs_uuid: None,
                size_bytes: 1 << 30,
            }),
    );

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(DiskDiscoveryController::new(backend)));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut ok = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let disks = state.list(NS, ResourceType::DiskStatus);
        let vols = state.list(NS, ResourceType::DiscoveredVolume);
        if disks.len() == 1 && vols.len() == 1 {
            if let Resource::DiscoveredVolume(v) = &vols[0].spec {
                if v.fs_type.as_deref() == Some("ext4") {
                    ok = true;
                    break;
                }
            }
        }
    }
    assert!(ok, "discovery did not populate disk + volume status");

    shutdown.cancel();
    let _ = handle.await;
}
