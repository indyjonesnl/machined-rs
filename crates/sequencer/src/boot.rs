//! The Boot sequence: mount filesystems, apply sysctls/hostname, start the
//! configured services.

use async_trait::async_trait;

use crate::task::{PhaseList, SequencerCtx, Task, TaskError};

struct MountFilesystems;

#[async_trait]
impl Task for MountFilesystems {
    fn name(&self) -> &str {
        "mount-filesystems"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        ctx.platform.mount_essential().map_err(|e| TaskError {
            task: self.name().into(),
            message: e.to_string(),
        })
    }
}

struct ApplySysctls;

#[async_trait]
impl Task for ApplySysctls {
    fn name(&self) -> &str {
        "apply-sysctls"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        for s in ctx.provider.sysctls() {
            ctx.platform
                .set_sysctl(&s.key, &s.value)
                .map_err(|e| TaskError {
                    task: self.name().into(),
                    message: e.to_string(),
                })?;
        }
        Ok(())
    }
}

struct SetHostname;

#[async_trait]
impl Task for SetHostname {
    fn name(&self) -> &str {
        "set-hostname"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        if let Some(name) = ctx.provider.hostname() {
            ctx.platform.set_hostname(name).map_err(|e| TaskError {
                task: self.name().into(),
                message: e.to_string(),
            })?;
        }
        Ok(())
    }
}

struct StartServices;

#[async_trait]
impl Task for StartServices {
    fn name(&self) -> &str {
        "start-services"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        let rt = ctx.provider.runtime();
        if !rt.disabled {
            // Best-effort: write the containerd config if absent.
            let path = std::path::Path::new(&rt.config_path);
            if !path.exists() {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(e) = std::fs::write(path, machined_config::containerd_config_toml(rt)) {
                    tracing::warn!("writing containerd config: {e}");
                }
            }
        }
        let services = machined_config::effective_services(rt, ctx.provider.services());
        let mut mgr = ctx.services.lock().await;
        mgr.start_all(&services, ctx.readiness.clone())
            .map_err(|message| TaskError {
                task: self.name().into(),
                message,
            })
    }
}

/// Build the Boot phase list.
pub fn boot_sequence() -> PhaseList {
    PhaseList::new()
        .phase(
            "early",
            vec![Box::new(MountFilesystems), Box::new(ApplySysctls)],
        )
        .phase("identity", vec![Box::new(SetHostname)])
        .phase("services", vec![Box::new(StartServices)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::SequencerCtx;
    use machined_config::{
        MachineConfig, MachineSection, Provider, RestartPolicy, RuntimeSection, ServiceConfig,
    };
    use machined_platform::{essential_mounts, FakePlatform};
    use machined_runtime_core::State;
    use machined_supervisor::{DefaultReadiness, ServiceManager};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn boot_mounts_and_starts_services() {
        let platform = Arc::new(FakePlatform::new());
        let state = State::new();
        let cfg = MachineConfig {
            machine: MachineSection {
                hostname: Some("node-1".into()),
                sysctls: vec![],
                services: vec![ServiceConfig {
                    id: "blip".into(),
                    command: vec!["true".into()],
                    depends_on: vec![],
                    restart: RestartPolicy::Never,
                    stop_grace_secs: None,
                }],
                network: Default::default(),
                install: None,
                time: Default::default(),
                // Hermetic test: never spawn the host containerd.
                runtime: RuntimeSection {
                    disabled: true,
                    ..Default::default()
                },
                pods: vec![],
            },
        };
        let ctx = SequencerCtx {
            state: state.clone(),
            platform: platform.clone(),
            provider: Provider::new(cfg),
            services: Arc::new(Mutex::new(ServiceManager::new(state.clone()))),
            readiness: Arc::new(DefaultReadiness),
        };

        boot_sequence().run(&ctx).await.unwrap();

        {
            let rec = platform.recorded.lock().unwrap();
            assert_eq!(rec.mounts.len(), essential_mounts().len());
            assert_eq!(rec.hostname.as_deref(), Some("node-1"));
        }

        // The service eventually publishes status.
        let k = machined_resources::Key::new(
            "runtime",
            machined_resources::ResourceType::ServiceStatus,
            "blip",
        );
        let mut seen = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if state.get(&k).is_ok() {
                seen = true;
                break;
            }
        }
        assert!(seen);

        ctx.services.lock().await.stop_all().await;
    }
}
