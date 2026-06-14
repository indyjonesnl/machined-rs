//! Reconciles config-declared pods via CRI and publishes PodStatus.
//! Depends only on the CriClient trait — runtime-pluggable by construction.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use machined_config::Provider;
use machined_cri::{ContainerSpec, ContainerState, CriClient, PodSpec};
use machined_resources::{
    Key, PodPhase, PodStatus, Resource, ResourceObject, ResourceType, RuntimeStatus,
};
use machined_runtime_core::{
    reconcile_owned, Controller, Input, InputKind, Output, OutputKind, ReconcileCtx,
};
use tracing::warn;

use super::NS;

const OWNER: &str = "pod-controller";

pub struct PodController {
    cri: Arc<dyn CriClient>,
    provider: Provider,
}

impl PodController {
    pub fn new(cri: Arc<dyn CriClient>, provider: Provider) -> Self {
        Self { cri, provider }
    }
}

fn status_obj(name: &str, phase: PodPhase, container_id: &str, message: &str) -> ResourceObject {
    ResourceObject::new(
        NS,
        name,
        Resource::PodStatus(PodStatus {
            name: name.to_string(),
            phase,
            container_id: container_id.to_string(),
            message: message.to_string(),
        }),
    )
}

fn runtime_ready(state: &machined_runtime_core::State) -> bool {
    matches!(
        state
            .get(&Key::new(NS, ResourceType::RuntimeStatus, "containerd"))
            .map(|o| o.spec),
        Ok(Resource::RuntimeStatus(RuntimeStatus { ready: true, .. }))
    )
}

#[async_trait]
impl Controller for PodController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.into(),
            typ: ResourceType::RuntimeStatus,
            kind: InputKind::Weak,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::PodStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    fn resync_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(5))
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let pods = self.provider.pods();
        if pods.is_empty() {
            // GC any stale PodStatus we previously owned.
            reconcile_owned(&ctx.state, OWNER, NS, ResourceType::PodStatus, vec![])?;
            return Ok(());
        }
        let ready = runtime_ready(&ctx.state);
        let mut desired = Vec::with_capacity(pods.len());
        for p in pods {
            if !ready {
                desired.push(status_obj(
                    &p.name,
                    PodPhase::Pending,
                    "",
                    "runtime not ready",
                ));
                continue;
            }
            desired.push(self.run_one(p).await);
        }
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::PodStatus, desired)?;
        Ok(())
    }
}

impl PodController {
    async fn run_one(&self, p: &machined_config::PodConfig) -> ResourceObject {
        // 1. image must be present (pre-imported on offline nodes).
        match self.cri.image_present(&p.image).await {
            Ok(true) => {}
            Ok(false) => return status_obj(&p.name, PodPhase::Pending, "", "image not present"),
            Err(e) => {
                warn!(pod = %p.name, error = %e, "image_present failed");
                return status_obj(&p.name, PodPhase::Pending, "", "cri unreachable");
            }
        }
        // 2. sandbox (idempotent by name).
        let sandbox = match self.ensure_sandbox(p).await {
            Ok(id) => id,
            Err(m) => return status_obj(&p.name, PodPhase::Pending, "", &m),
        };
        // 3. container (idempotent by name within the sandbox).
        let container = match self.ensure_container(&sandbox, p).await {
            Ok(id) => id,
            Err(m) => return status_obj(&p.name, PodPhase::Pending, "", &m),
        };
        // 4. observe + report.
        match self.cri.container_state(&container).await {
            Ok(ContainerState::Running) => status_obj(&p.name, PodPhase::Running, &container, ""),
            Ok(ContainerState::Exited) => {
                status_obj(&p.name, PodPhase::Failed, &container, "container exited")
            }
            Ok(_) => status_obj(&p.name, PodPhase::Pending, &container, "starting"),
            Err(e) => status_obj(&p.name, PodPhase::Pending, &container, &e.to_string()),
        }
    }

    async fn ensure_sandbox(
        &self,
        p: &machined_config::PodConfig,
    ) -> std::result::Result<String, String> {
        if let Some(id) = self
            .cri
            .find_sandbox(&p.name)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(id);
        }
        let spec = PodSpec {
            name: p.name.clone(),
            uid: format!("uid-{}", p.name),
            host_network: p.host_network,
        };
        self.cri
            .run_pod_sandbox(&spec)
            .await
            .map_err(|e| e.to_string())
    }

    async fn ensure_container(
        &self,
        sandbox: &str,
        p: &machined_config::PodConfig,
    ) -> std::result::Result<String, String> {
        if let Some(id) = self
            .cri
            .find_container(sandbox, &p.name)
            .await
            .map_err(|e| e.to_string())?
        {
            // start if still in Created.
            if matches!(
                self.cri.container_state(&id).await,
                Ok(ContainerState::Created)
            ) {
                self.cri
                    .start_container(&id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            return Ok(id);
        }
        let cspec = ContainerSpec {
            name: p.name.clone(),
            image: p.image.clone(),
            command: p.command.clone(),
            args: p.args.clone(),
        };
        let id = self
            .cri
            .create_container(sandbox, &cspec)
            .await
            .map_err(|e| e.to_string())?;
        self.cri
            .start_container(&id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::{MachineConfig, MachineSection, PodConfig};
    use machined_cri::FakeCriClient;
    use machined_runtime_core::State;

    fn provider_with_pod(host_network: bool) -> Provider {
        Provider::new(MachineConfig {
            machine: MachineSection {
                pods: vec![PodConfig {
                    name: "hello".into(),
                    image: "busybox:1.36".into(),
                    command: vec!["/bin/sh".into(), "-c".into()],
                    args: vec!["sleep 3600".into()],
                    host_network,
                }],
                ..Default::default()
            },
        })
    }

    fn pod_status(state: &State, name: &str) -> PodStatus {
        match state
            .get(&Key::new(NS, ResourceType::PodStatus, name))
            .unwrap()
            .spec
        {
            Resource::PodStatus(p) => p,
            _ => panic!("wrong type"),
        }
    }

    fn mark_ready(state: &State) {
        let _ = state.create(ResourceObject::new(
            NS,
            "containerd",
            Resource::RuntimeStatus(RuntimeStatus {
                ready: true,
                name: "containerd".into(),
                version: "2".into(),
            }),
        ));
    }

    #[tokio::test]
    async fn pending_when_runtime_not_ready() {
        let cri = Arc::new(
            FakeCriClient::new()
                .with_version("c", "2")
                .with_image("busybox:1.36"),
        );
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = PodController::new(cri, provider_with_pod(true));
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(pod_status(&state, "hello").phase, PodPhase::Pending);
    }

    #[tokio::test]
    async fn runs_pod_when_ready_and_image_present() {
        let cri = Arc::new(
            FakeCriClient::new()
                .with_version("c", "2")
                .with_image("busybox:1.36"),
        );
        let state = State::new();
        mark_ready(&state);
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = PodController::new(cri.clone(), provider_with_pod(true));
        c.reconcile(&ctx).await.unwrap();
        let st = pod_status(&state, "hello");
        assert_eq!(st.phase, PodPhase::Running);
        assert!(!st.container_id.is_empty());
        assert_eq!(cri.sandbox_count(), 1);
        // Idempotent: a second reconcile must not create a second sandbox.
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(cri.sandbox_count(), 1);
    }

    #[tokio::test]
    async fn pending_when_image_absent() {
        let cri = Arc::new(FakeCriClient::new().with_version("c", "2")); // no image
        let state = State::new();
        mark_ready(&state);
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = PodController::new(cri, provider_with_pod(true));
        c.reconcile(&ctx).await.unwrap();
        let st = pod_status(&state, "hello");
        assert_eq!(st.phase, PodPhase::Pending);
        assert_eq!(st.message, "image not present");
    }
}
