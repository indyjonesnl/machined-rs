//! The Shutdown sequence: stop services in reverse order, then unmount/halt.
//! (Unmount + halt are no-ops on the fake platform and full ops under Linux;
//! M5 fleshes out disk teardown and final reboot/poweroff.)

use async_trait::async_trait;

use crate::task::{PhaseList, SequencerCtx, Task};

struct StopServices;

#[async_trait]
impl Task for StopServices {
    fn name(&self) -> &str {
        "stop-services"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        ctx.services.lock().await.stop_all().await;
        Ok(())
    }
}

struct SyncAndUnmount;

#[async_trait]
impl Task for SyncAndUnmount {
    fn name(&self) -> &str {
        "sync-and-unmount"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        ctx.platform.sync();
        // Reverse mount order; best-effort — shutdown must complete.
        for target in ["/var", "/system/state", "/boot"] {
            match ctx.platform.is_mounted(target) {
                Ok(true) => {
                    if let Err(e) = ctx.platform.unmount(target) {
                        tracing::warn!("unmount {target}: {e}; retrying lazy (MNT_DETACH)");
                        if let Err(e2) = ctx.platform.unmount_lazy(target) {
                            tracing::warn!("lazy unmount {target}: {e2}");
                        }
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("is_mounted {target}: {e}"),
            }
        }
        Ok(())
    }
}

/// Build the Shutdown phase list.
pub fn shutdown_sequence() -> PhaseList {
    PhaseList::new()
        .phase("stop", vec![Box::new(StopServices)])
        .phase("disk", vec![Box::new(SyncAndUnmount)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::{MachineConfig, Provider};
    use machined_platform::{FakePlatform, MountSpec, Platform};
    use machined_runtime_core::State;
    use machined_supervisor::{DefaultReadiness, ServiceManager};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn shutdown_runs_clean() {
        let state = State::new();
        let platform = Arc::new(FakePlatform::new());
        // Pre-mount /var and /system/state so the disk phase has work to do.
        for (source, target) in [("/dev/sda2", "/var"), ("/dev/sda3", "/system/state")] {
            platform
                .mount(&MountSpec {
                    source: source.into(),
                    target: target.into(),
                    fstype: "ext4".into(),
                    flags: 0,
                    data: None,
                })
                .unwrap();
        }
        let ctx = SequencerCtx {
            state: state.clone(),
            platform: platform.clone(),
            provider: Provider::new(MachineConfig::default()),
            services: Arc::new(Mutex::new(ServiceManager::new(state))),
            readiness: Arc::new(DefaultReadiness),
        };
        shutdown_sequence().run(&ctx).await.unwrap();

        let rec = platform.recorded.lock().unwrap();
        assert_eq!(rec.syncs, 1);
        // Reverse mount order, /boot was never mounted so it is untouched.
        assert_eq!(rec.unmounts, vec!["/var", "/system/state"]);
        // The interleaved log pins sync STRICTLY BEFORE the unmounts.
        assert_eq!(
            rec.disk_ops,
            vec!["sync", "unmount:/var", "unmount:/system/state"]
        );
    }

    #[tokio::test]
    async fn busy_unmount_escalates_to_lazy() {
        let state = State::new();
        let platform = Arc::new(FakePlatform::new());
        for (source, target) in [("/dev/sda2", "/var"), ("/dev/sda3", "/system/state")] {
            platform
                .mount(&MountSpec {
                    source: source.into(),
                    target: target.into(),
                    fstype: "ext4".into(),
                    flags: 0,
                    data: None,
                })
                .unwrap();
        }
        // /var is busy: its plain unmount fails, forcing the lazy escalation.
        platform
            .fail_unmount_targets
            .lock()
            .unwrap()
            .push("/var".into());
        let ctx = SequencerCtx {
            state: state.clone(),
            platform: platform.clone(),
            provider: Provider::new(MachineConfig::default()),
            services: Arc::new(Mutex::new(ServiceManager::new(state))),
            readiness: Arc::new(DefaultReadiness),
        };
        shutdown_sequence().run(&ctx).await.unwrap();

        let rec = platform.recorded.lock().unwrap();
        assert_eq!(
            rec.disk_ops,
            vec!["sync", "unmount_lazy:/var", "unmount:/system/state"]
        );
    }
}
