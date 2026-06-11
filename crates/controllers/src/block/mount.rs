//! Mounts provisioned system volumes at their fixed mountpoints.

use std::sync::Arc;

use async_trait::async_trait;
use machined_platform::{MountSpec, Platform};
use machined_resources::{MountStatus, Resource, ResourceObject, ResourceType, VolumePhase};
use machined_runtime_core::{
    reconcile_owned, Controller, Input, InputKind, Output, OutputKind, ReconcileCtx,
};

use super::{ctl, NS};

const OWNER: &str = "volume-mount";

/// Fixed mountpoint for each system volume label; `None` for anything else.
pub fn mountpoint(label: &str) -> Option<&'static str> {
    match label {
        "EFI" => Some("/boot"),
        "STATE" => Some("/system/state"),
        "EPHEMERAL" => Some("/var"),
        _ => None,
    }
}

pub struct VolumeMountController {
    platform: Arc<dyn Platform>,
}

impl VolumeMountController {
    pub fn new(platform: Arc<dyn Platform>) -> Self {
        Self { platform }
    }
}

#[async_trait]
impl Controller for VolumeMountController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::VolumeStatus,
            kind: InputKind::Weak,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::MountStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let mut statuses = Vec::new();
        for obj in ctx.state.list(NS, ResourceType::VolumeStatus) {
            let Resource::VolumeStatus(v) = obj.spec else {
                continue;
            };
            if v.phase != VolumePhase::Provisioned {
                continue;
            }
            let Some(target) = mountpoint(&v.label) else {
                continue;
            };

            if !self.platform.is_mounted(target).map_err(ctl)? {
                self.platform
                    .mount(&MountSpec {
                        source: v.device.clone(),
                        target: target.to_string(),
                        fstype: v.fs.clone(),
                        flags: 0,
                        data: None,
                    })
                    .map_err(ctl)?;
            }

            statuses.push(ResourceObject::new(
                NS,
                &v.label,
                Resource::MountStatus(MountStatus {
                    volume: v.label.clone(),
                    source: v.device,
                    target: target.to_string(),
                    fstype: v.fs,
                    mounted: true,
                }),
            ));
        }
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::MountStatus, statuses)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_platform::FakePlatform;
    use machined_resources::VolumeStatus;
    use machined_runtime_core::{ReconcileCtx, State};

    #[tokio::test]
    async fn skips_already_mounted_target() {
        let platform = Arc::new(FakePlatform::new());
        // /var is already mounted (e.g. from a prior boot) before we reconcile.
        platform
            .mount(&MountSpec {
                source: "/dev/other".into(),
                target: "/var".into(),
                fstype: "ext4".into(),
                flags: 0,
                data: None,
            })
            .unwrap();
        let state = State::new();
        seed_volume(&state, "EPHEMERAL", VolumePhase::Provisioned); // maps to /var
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeMountController::new(platform.clone());
        c.reconcile(&ctx).await.unwrap();

        // No NEW mount issued (still just the pre-existing one); MountStatus published.
        assert_eq!(platform.recorded.lock().unwrap().mounts.len(), 1);
        assert_eq!(state.list(NS, ResourceType::MountStatus).len(), 1);
    }

    fn seed_volume(state: &State, label: &str, phase: VolumePhase) {
        state
            .create(ResourceObject::new(
                NS,
                label,
                Resource::VolumeStatus(VolumeStatus {
                    name: label.into(),
                    device: format!("/dev/sda-{label}"),
                    fs: "ext4".into(),
                    label: label.into(),
                    phase,
                }),
            ))
            .unwrap();
    }

    #[test]
    fn mountpoint_map() {
        assert_eq!(mountpoint("EFI"), Some("/boot"));
        assert_eq!(mountpoint("STATE"), Some("/system/state"));
        assert_eq!(mountpoint("EPHEMERAL"), Some("/var"));
        assert_eq!(mountpoint("DATA"), None);
    }

    #[tokio::test]
    async fn mounts_provisioned_volumes_idempotently() {
        let platform = Arc::new(FakePlatform::new());
        let state = State::new();
        seed_volume(&state, "EFI", VolumePhase::Provisioned);
        seed_volume(&state, "STATE", VolumePhase::Provisioned);
        seed_volume(&state, "EPHEMERAL", VolumePhase::Provisioned);
        // A non-system volume that must be ignored.
        seed_volume(&state, "DATA", VolumePhase::Provisioned);

        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeMountController::new(platform.clone());
        c.reconcile(&ctx).await.unwrap();

        // Three system volumes mounted; DATA ignored.
        assert_eq!(platform.recorded.lock().unwrap().mounts.len(), 3);
        assert_eq!(state.list(NS, ResourceType::MountStatus).len(), 3);

        // Second reconcile: all already mounted → no new mount calls.
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(platform.recorded.lock().unwrap().mounts.len(), 3);
    }

    #[tokio::test]
    async fn skips_unprovisioned() {
        let platform = Arc::new(FakePlatform::new());
        let state = State::new();
        seed_volume(&state, "STATE", VolumePhase::Failed);
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeMountController::new(platform.clone());
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(platform.recorded.lock().unwrap().mounts.len(), 0);
        assert_eq!(state.list(NS, ResourceType::MountStatus).len(), 0);
    }
}
