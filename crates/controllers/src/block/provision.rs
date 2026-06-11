//! The block provisioning controller and its pure safety guard.

use std::sync::Arc;

use async_trait::async_trait;
use machined_block::{BlockProvisioner, FsType, PartType, PartitionPlan};
use machined_config::Provider;
use machined_resources::{
    DiscoveredVolume, Resource, ResourceObject, ResourceType, VolumePhase, VolumeStatus,
};
use machined_runtime_core::{
    reconcile_owned, Controller, Input, InputKind, Output, OutputKind, ReconcileCtx,
};
use tracing::{error, info};

use super::{ctl, NS};

const OWNER: &str = "volume-provisioner";

/// The fixed labels this OS lays out.
const LABELS: [&str; 3] = ["EFI", "STATE", "EPHEMERAL"];

/// The decision the safety guard reaches about an install disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvisionDecision {
    /// The disk already carries our exact layout — nothing to do.
    Skip,
    /// The disk is blank, or wipe was requested — lay out fresh.
    Provision,
    /// The disk has foreign data and wipe was not requested — refuse.
    RefuseForeign,
}

/// Decide what to do with `install_disk`, given the discovered volumes and the
/// wipe flag. PURE — no I/O. This is the single source of the destructive
/// decision.
pub fn plan_provisioning(
    install_disk: &str,
    wipe: bool,
    discovered: &[DiscoveredVolume],
) -> ProvisionDecision {
    // Match a discovered volume to this disk by EXACT parent-disk name (the bare
    // name SysfsBlock reports, e.g. "sda") or exact device path. Exact equality
    // is deliberate: "sda" must never match "sda1"/"sdaa", and a missed match on
    // a foreign disk could make it look blank and get wiped. (The `device ==`
    // arm is a belt-and-suspenders match for callers that pass a whole-disk path
    // as a device; it never collides since device paths are partition paths.)
    let leaf = install_disk.rsplit('/').next().unwrap_or(install_disk);
    let on_disk: Vec<&DiscoveredVolume> = discovered
        .iter()
        .filter(|v| v.disk == leaf || v.device == install_disk)
        .collect();

    if on_disk.is_empty() {
        return ProvisionDecision::Provision; // blank disk
    }

    // "Ours" is decided by EXACT label-set equality (no more, no less than our
    // three labels). Labels are the sole trust anchor here: a disk carrying any
    // foreign/extra label is treated as foreign (RefuseForeign unless wipe).
    let labels: Vec<&str> = on_disk.iter().map(|v| v.partition_label.as_str()).collect();
    let is_ours =
        LABELS.iter().all(|l| labels.contains(l)) && labels.iter().all(|l| LABELS.contains(l));
    if is_ours {
        return ProvisionDecision::Skip;
    }

    if wipe {
        ProvisionDecision::Provision
    } else {
        ProvisionDecision::RefuseForeign
    }
}

/// The fixed GPT layout this OS provisions.
pub fn fixed_layout() -> Vec<PartitionPlan> {
    vec![
        PartitionPlan {
            label: "EFI".into(),
            part_type: PartType::EfiSystem,
            fs: FsType::Vfat,
            size_bytes: 512 * 1024 * 1024,
        },
        PartitionPlan {
            label: "STATE".into(),
            part_type: PartType::LinuxFilesystem,
            fs: FsType::Ext4,
            size_bytes: 1024 * 1024 * 1024,
        },
        PartitionPlan {
            label: "EPHEMERAL".into(),
            part_type: PartType::LinuxFilesystem,
            fs: FsType::Ext4,
            size_bytes: 0, // rest
        },
    ]
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    fn vol(disk: &str, label: &str) -> DiscoveredVolume {
        DiscoveredVolume {
            device: format!("/dev/{disk}1"),
            disk: disk.into(),
            partition_uuid: "u".into(),
            partition_label: label.into(),
            partition_type_guid: "g".into(),
            fs_type: None,
            fs_label: None,
            fs_uuid: None,
            size_bytes: 1 << 20,
        }
    }

    #[test]
    fn blank_disk_provisions() {
        assert_eq!(
            plan_provisioning("/dev/sda", false, &[]),
            ProvisionDecision::Provision
        );
    }

    #[test]
    fn our_exact_layout_skips() {
        let d = vec![
            vol("sda", "EFI"),
            vol("sda", "STATE"),
            vol("sda", "EPHEMERAL"),
        ];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::Skip
        );
    }

    #[test]
    fn foreign_no_wipe_refuses() {
        let d = vec![vol("sda", "WINDOWS")];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::RefuseForeign
        );
    }

    #[test]
    fn foreign_with_wipe_provisions() {
        let d = vec![vol("sda", "WINDOWS")];
        assert_eq!(
            plan_provisioning("/dev/sda", true, &d),
            ProvisionDecision::Provision
        );
    }

    #[test]
    fn partial_our_layout_is_foreign() {
        // Only STATE present (missing EFI/EPHEMERAL) → not our exact layout.
        let d = vec![vol("sda", "STATE")];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::RefuseForeign
        );
    }

    #[test]
    fn volumes_on_other_disk_ignored() {
        let d = vec![vol("sdb", "WINDOWS")];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::Provision
        );
    }

    #[test]
    fn our_layout_plus_foreign_partition_refuses() {
        // Our 3 labels PLUS a foreign 4th → NOT our exact set → refuse (not Skip).
        let d = vec![
            vol("sda", "EFI"),
            vol("sda", "STATE"),
            vol("sda", "EPHEMERAL"),
            vol("sda", "RECOVERY"),
        ];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::RefuseForeign
        );
    }

    #[test]
    fn similar_disk_name_does_not_match() {
        // Regression guard: a parent-disk filter must use EXACT equality, so
        // "sda" must NOT match "sda1"/"sdaa". Neither volume is on /dev/sda, so
        // the disk reads as blank → Provision (crucially NOT RefuseForeign by
        // an accidental substring match).
        let d = vec![vol("sdaa", "WINDOWS"), vol("sda1", "DATA")];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::Provision
        );
    }

    #[test]
    fn our_layout_with_foreign_on_other_disk_skips() {
        // Our layout on the target disk + foreign data on another disk → the
        // filter isolates the target, so the target is Skip.
        let d = vec![
            vol("sda", "EFI"),
            vol("sda", "STATE"),
            vol("sda", "EPHEMERAL"),
            vol("sdb", "WINDOWS"),
        ];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::Skip
        );
    }

    #[test]
    fn nvme_leaf_extraction() {
        // The `p`-separator device family: leaf of /dev/nvme0n1 is "nvme0n1".
        let d = vec![vol("nvme0n1", "WINDOWS")];
        assert_eq!(
            plan_provisioning("/dev/nvme0n1", false, &d),
            ProvisionDecision::RefuseForeign
        );
    }
}

pub struct VolumeProvisionerController {
    backend: Arc<dyn BlockProvisioner>,
    provider: Provider,
}

impl VolumeProvisionerController {
    pub fn new(backend: Arc<dyn BlockProvisioner>, provider: Provider) -> Self {
        Self { backend, provider }
    }
}

#[async_trait]
impl Controller for VolumeProvisionerController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        // Re-evaluate when discovery changes.
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::DiscoveredVolume,
            kind: InputKind::Weak,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::VolumeStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let Some(install) = self.provider.install() else {
            return Ok(()); // no install target configured
        };
        let disk = install.disk.clone();

        let discovered: Vec<DiscoveredVolume> = ctx
            .state
            .list(NS, ResourceType::DiscoveredVolume)
            .into_iter()
            .filter_map(|o| match o.spec {
                Resource::DiscoveredVolume(v) => Some(v),
                _ => None,
            })
            .collect();

        match plan_provisioning(&disk, install.wipe, &discovered) {
            ProvisionDecision::RefuseForeign => {
                error!(disk = %disk, "refusing to provision: disk has foreign data and wipe is false");
                return Err(ctl(format!(
                    "install disk {disk} has foreign data; set install.wipe to overwrite"
                )));
            }
            ProvisionDecision::Skip => {
                info!(disk = %disk, "install disk already provisioned");
                let vols = provisioned_status_from_discovered(&disk, &discovered);
                reconcile_owned(&ctx.state, OWNER, NS, ResourceType::VolumeStatus, vols)?;
            }
            ProvisionDecision::Provision => {
                info!(disk = %disk, wipe = install.wipe, "provisioning install disk");
                if !discovered.is_empty() && install.wipe {
                    self.backend.wipe(&disk).await.map_err(ctl)?;
                }
                let layout = fixed_layout();
                let devices = self
                    .backend
                    .create_partitions(&disk, &layout)
                    .await
                    .map_err(ctl)?;
                let mut statuses = Vec::new();
                for (plan, device) in layout.iter().zip(devices.iter()) {
                    self.backend
                        .format(device, plan.fs, &plan.label)
                        .await
                        .map_err(ctl)?;
                    statuses.push(volume_status_obj(
                        &plan.label,
                        device,
                        plan.fs.as_str(),
                        &plan.label,
                        VolumePhase::Provisioned,
                    ));
                }
                reconcile_owned(&ctx.state, OWNER, NS, ResourceType::VolumeStatus, statuses)?;
            }
        }
        Ok(())
    }
}

fn volume_status_obj(
    name: &str,
    device: &str,
    fs: &str,
    label: &str,
    phase: VolumePhase,
) -> ResourceObject {
    ResourceObject::new(
        NS,
        name,
        Resource::VolumeStatus(VolumeStatus {
            name: name.to_string(),
            device: device.to_string(),
            fs: fs.to_string(),
            label: label.to_string(),
            phase,
        }),
    )
}

/// Build VolumeStatus for an already-provisioned disk from discovery.
fn provisioned_status_from_discovered(
    disk: &str,
    discovered: &[DiscoveredVolume],
) -> Vec<ResourceObject> {
    let leaf = disk.rsplit('/').next().unwrap_or(disk);
    discovered
        .iter()
        .filter(|v| v.disk == leaf)
        .filter(|v| LABELS.contains(&v.partition_label.as_str()))
        .map(|v| {
            volume_status_obj(
                &v.partition_label,
                &v.device,
                v.fs_type.as_deref().unwrap_or(""),
                &v.partition_label,
                VolumePhase::Provisioned,
            )
        })
        .collect()
}

#[cfg(test)]
mod controller_tests {
    use super::*;
    use machined_block::{DiskInfo, FakeBlockBackend};
    use machined_config::{InstallSection, MachineConfig, MachineSection};
    use machined_resources::Resource as Res;
    use machined_runtime_core::{ReconcileCtx, State};

    fn provider(disk: &str, wipe: bool) -> Provider {
        Provider::new(MachineConfig {
            machine: MachineSection {
                hostname: None,
                sysctls: vec![],
                services: vec![],
                network: Default::default(),
                install: Some(InstallSection {
                    disk: disk.into(),
                    wipe,
                }),
            },
        })
    }

    fn seed_discovered(state: &State, disk: &str, label: &str) {
        state
            .create(ResourceObject::new(
                NS,
                format!("{disk}-{label}"),
                Res::DiscoveredVolume(DiscoveredVolume {
                    device: format!("/dev/{disk}1"),
                    disk: disk.into(),
                    partition_uuid: "u".into(),
                    partition_label: label.into(),
                    partition_type_guid: "g".into(),
                    fs_type: None,
                    fs_label: None,
                    fs_uuid: None,
                    size_bytes: 1 << 20,
                }),
            ))
            .unwrap();
    }

    #[tokio::test]
    async fn blank_disk_gets_provisioned() {
        let backend = Arc::new(FakeBlockBackend::new().with_disk(DiskInfo {
            name: "sda".into(),
            path: "/dev/sda".into(),
            size_bytes: 8 << 30,
            model: "M".into(),
            serial: "S".into(),
            rotational: false,
            read_only: false,
        }));
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeProvisionerController::new(backend.clone(), provider("/dev/sda", false));
        c.reconcile(&ctx).await.unwrap();

        // Three partitions created + formatted; three VolumeStatus published.
        assert_eq!(backend.creates(), vec!["/dev/sda".to_string()]);
        assert_eq!(backend.formats().len(), 3);
        assert_eq!(state.list(NS, ResourceType::VolumeStatus).len(), 3);
    }

    #[tokio::test]
    async fn foreign_disk_without_wipe_makes_no_destructive_call() {
        let backend = Arc::new(FakeBlockBackend::new());
        let state = State::new();
        seed_discovered(&state, "sda", "WINDOWS");
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeProvisionerController::new(backend.clone(), provider("/dev/sda", false));
        let res = c.reconcile(&ctx).await;

        assert!(res.is_err(), "refuses foreign disk");
        // CRITICAL: no destructive operation was performed.
        assert!(backend.wipes().is_empty());
        assert!(backend.creates().is_empty());
        assert!(backend.formats().is_empty());
        assert_eq!(state.list(NS, ResourceType::VolumeStatus).len(), 0);
    }

    #[tokio::test]
    async fn idempotent_second_reconcile_skips() {
        // First provision the (fake) disk via the controller, then re-run with
        // discovery reflecting our layout → Skip (no second create).
        let backend = Arc::new(FakeBlockBackend::new());
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeProvisionerController::new(backend.clone(), provider("/dev/sda", false));
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(backend.creates().len(), 1);

        // Simulate discovery seeing our layout now.
        for label in ["EFI", "STATE", "EPHEMERAL"] {
            seed_discovered(&state, "sda", label);
        }
        c.reconcile(&ctx).await.unwrap();
        // Still only one create — the second pass Skipped.
        assert_eq!(backend.creates().len(), 1);
    }

    #[tokio::test]
    async fn foreign_with_wipe_wipes_then_provisions() {
        let backend = Arc::new(FakeBlockBackend::new());
        let state = State::new();
        seed_discovered(&state, "sda", "WINDOWS");
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeProvisionerController::new(backend.clone(), provider("/dev/sda", true));
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(backend.wipes(), vec!["/dev/sda".to_string()]);
        assert_eq!(backend.creates().len(), 1);
        assert_eq!(backend.formats().len(), 3);
    }
}
