//! Periodically probes the CRI socket and publishes RuntimeStatus.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use machined_config::Provider;
use machined_cri::CriClient;
use machined_resources::{Resource, ResourceObject, ResourceType, RuntimeStatus};
use machined_runtime_core::{reconcile_owned, Controller, Input, Output, OutputKind, ReconcileCtx};
use tracing::warn;

use super::NS;

const OWNER: &str = "runtime-health";

pub struct RuntimeHealthController {
    cri: Arc<dyn CriClient>,
    provider: Provider,
}

impl RuntimeHealthController {
    pub fn new(cri: Arc<dyn CriClient>, provider: Provider) -> Self {
        Self { cri, provider }
    }
}

fn status_obj(ready: bool, name: &str, version: &str) -> ResourceObject {
    ResourceObject::new(
        NS,
        "containerd",
        Resource::RuntimeStatus(RuntimeStatus {
            ready,
            name: name.to_string(),
            version: version.to_string(),
        }),
    )
}

#[async_trait]
impl Controller for RuntimeHealthController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        Vec::new()
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::RuntimeStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    fn resync_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(10))
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        if self.provider.runtime().disabled {
            reconcile_owned(
                &ctx.state,
                OWNER,
                NS,
                ResourceType::RuntimeStatus,
                vec![status_obj(false, "", "")],
            )?;
            return Ok(());
        }

        let (ready, name, version) = match (self.cri.ready().await, self.cri.version().await) {
            (Ok(ready), Ok(v)) => (ready, v.runtime_name, v.runtime_version),
            (r, v) => {
                let e = r
                    .err()
                    .map(|e| e.to_string())
                    .or(v.err().map(|e| e.to_string()));
                warn!(error = ?e, "cri probe failed; runtime not ready");
                (false, String::new(), String::new())
            }
        };
        reconcile_owned(
            &ctx.state,
            OWNER,
            NS,
            ResourceType::RuntimeStatus,
            vec![status_obj(ready, &name, &version)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::{MachineConfig, MachineSection, RuntimeSection};
    use machined_cri::FakeCriClient;
    use machined_resources::Key;
    use machined_runtime_core::{ReconcileCtx, State};

    fn provider(disabled: bool) -> Provider {
        Provider::new(MachineConfig {
            machine: MachineSection {
                hostname: None,
                sysctls: vec![],
                services: vec![],
                network: Default::default(),
                install: None,
                time: Default::default(),
                runtime: RuntimeSection {
                    disabled,
                    ..Default::default()
                },
            },
        })
    }

    fn runtime_status(state: &State) -> RuntimeStatus {
        match state
            .get(&Key::new(NS, ResourceType::RuntimeStatus, "containerd"))
            .unwrap()
            .spec
        {
            Resource::RuntimeStatus(r) => r,
            _ => panic!("wrong type"),
        }
    }

    #[tokio::test]
    async fn publishes_ready_runtime() {
        let cri = Arc::new(
            FakeCriClient::new()
                .with_version("containerd", "2.0.0")
                .with_ready(true),
        );
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = RuntimeHealthController::new(cri, provider(false));
        c.reconcile(&ctx).await.unwrap();
        let st = runtime_status(&state);
        assert!(st.ready);
        assert_eq!(st.name, "containerd");
        assert_eq!(st.version, "2.0.0");
    }

    #[tokio::test]
    async fn runtime_not_ready_is_published() {
        let cri = Arc::new(
            FakeCriClient::new()
                .with_version("containerd", "2.0.0")
                .with_ready(false),
        );
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = RuntimeHealthController::new(cri, provider(false));
        c.reconcile(&ctx).await.unwrap();
        assert!(!runtime_status(&state).ready);
    }

    #[tokio::test]
    async fn unreachable_is_transient_not_error() {
        let cri = Arc::new(FakeCriClient::new()); // no version → errors
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = RuntimeHealthController::new(cri, provider(false));
        c.reconcile(&ctx).await.unwrap(); // Ok, not Err
        assert!(!runtime_status(&state).ready);
    }

    #[tokio::test]
    async fn disabled_does_not_probe() {
        let cri = Arc::new(
            FakeCriClient::new()
                .with_version("containerd", "2.0.0")
                .with_ready(true),
        );
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = RuntimeHealthController::new(cri.clone(), provider(true));
        c.reconcile(&ctx).await.unwrap();
        assert!(!runtime_status(&state).ready);
        assert_eq!(cri.calls(), 0);
    }
}
