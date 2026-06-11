//! Enumerates block devices via the `BlockBackend` and publishes `DiskStatus`
//! and `DiscoveredVolume` resources, GC'ing devices that have disappeared.

use std::sync::Arc;

use async_trait::async_trait;
use machined_block::BlockBackend;
use machined_resources::{DiscoveredVolume, DiskStatus, Resource, ResourceObject, ResourceType};
use machined_runtime_core::{reconcile_owned, Controller, Input, Output, OutputKind, ReconcileCtx};

use super::{ctl, NS};

const OWNER: &str = "disk-discovery";

pub struct DiskDiscoveryController {
    backend: Arc<dyn BlockBackend>,
}

impl DiskDiscoveryController {
    pub fn new(backend: Arc<dyn BlockBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Controller for DiskDiscoveryController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        // Boot-time run-once: the startup reconcile enumerates. Hotplug refresh
        // is a later milestone.
        Vec::new()
    }

    fn outputs(&self) -> Vec<Output> {
        [ResourceType::DiskStatus, ResourceType::DiscoveredVolume]
            .into_iter()
            .map(|typ| Output {
                typ,
                kind: OutputKind::Exclusive,
            })
            .collect()
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        // Fetch both, then publish DiscoveredVolume BEFORE DiskStatus with no
        // await between the two reconcile_owned calls — making DiskStatus the
        // "scan complete" barrier the provisioner gates on (DiskStatus present ⇒
        // this scan's DiscoveredVolumes are already in the store).
        let disks = self.backend.list_disks().await.map_err(ctl)?;
        let volumes = self.backend.list_volumes().await.map_err(ctl)?;
        let disk_objs = disks
            .into_iter()
            .map(|d| {
                ResourceObject::new(
                    NS,
                    &d.name,
                    Resource::DiskStatus(DiskStatus {
                        name: d.name.clone(),
                        path: d.path,
                        size_bytes: d.size_bytes,
                        model: d.model,
                        serial: d.serial,
                        rotational: d.rotational,
                        read_only: d.read_only,
                    }),
                )
            })
            .collect();
        let vol_objs = volumes
            .into_iter()
            .map(|v| {
                let id = leaf(&v.device);
                ResourceObject::new(
                    NS,
                    &id,
                    Resource::DiscoveredVolume(DiscoveredVolume {
                        device: v.device.clone(),
                        disk: v.disk,
                        partition_uuid: v.partition_uuid,
                        partition_label: v.partition_label,
                        partition_type_guid: v.partition_type_guid,
                        fs_type: v.fs_type.map(|t| t.as_str().to_string()),
                        fs_label: v.fs_label,
                        fs_uuid: v.fs_uuid,
                        size_bytes: v.size_bytes,
                    }),
                )
            })
            .collect();
        // Content first, then the DiskStatus barrier (no await between).
        reconcile_owned(
            &ctx.state,
            OWNER,
            NS,
            ResourceType::DiscoveredVolume,
            vol_objs,
        )?;
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::DiskStatus, disk_objs)?;
        Ok(())
    }
}

/// Resource id for a volume: the device leaf (`/dev/sda1` -> `sda1`).
fn leaf(device: &str) -> String {
    device.rsplit('/').next().unwrap_or(device).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_block::{DiskInfo, FakeBlockBackend, FsType, VolumeInfo};
    use machined_runtime_core::{ReconcileCtx, State};

    fn disk(name: &str) -> DiskInfo {
        DiskInfo {
            name: name.into(),
            path: format!("/dev/{name}"),
            size_bytes: 1 << 30,
            model: "M".into(),
            serial: "S".into(),
            rotational: false,
            read_only: false,
        }
    }

    fn vol(disk: &str, dev: &str) -> VolumeInfo {
        VolumeInfo {
            device: format!("/dev/{dev}"),
            disk: disk.into(),
            partition_uuid: "u".into(),
            partition_label: "STATE".into(),
            partition_type_guid: "g".into(),
            fs_type: Some(FsType::Ext4),
            fs_label: None,
            fs_uuid: None,
            size_bytes: 1 << 20,
        }
    }

    #[tokio::test]
    async fn publishes_disks_and_volumes() {
        let backend = Arc::new(
            FakeBlockBackend::new()
                .with_disk(disk("sda"))
                .with_volume(vol("sda", "sda1")),
        );
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = DiskDiscoveryController::new(backend);
        c.reconcile(&ctx).await.unwrap();

        assert_eq!(state.list(NS, ResourceType::DiskStatus).len(), 1);
        let vols = state.list(NS, ResourceType::DiscoveredVolume);
        assert_eq!(vols.len(), 1);
        match &vols[0].spec {
            Resource::DiscoveredVolume(v) => assert_eq!(v.fs_type.as_deref(), Some("ext4")),
            _ => panic!("wrong type"),
        }
    }

    // Exercises reconcile_owned's GC machinery across two manual reconcile
    // passes. NOTE: at runtime this controller has `inputs=[]` and reconciles
    // only once at boot, so a device disappearing live is not GC'd until a
    // re-trigger exists — hotplug/udev refresh is a documented M2b-2 deferral.
    #[tokio::test]
    async fn gcs_disappeared_devices() {
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        // First pass sees sda.
        let mut c1 =
            DiskDiscoveryController::new(Arc::new(FakeBlockBackend::new().with_disk(disk("sda"))));
        c1.reconcile(&ctx).await.unwrap();
        assert_eq!(state.list(NS, ResourceType::DiskStatus).len(), 1);

        // Second pass: sda gone → GC'd (no finalizers).
        let mut c2 = DiskDiscoveryController::new(Arc::new(FakeBlockBackend::new()));
        c2.reconcile(&ctx).await.unwrap();
        assert_eq!(state.list(NS, ResourceType::DiskStatus).len(), 0);
    }
}
