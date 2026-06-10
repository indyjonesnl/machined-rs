//! Applies the desired `HostnameSpec` via the platform.

use std::sync::Arc;

use async_trait::async_trait;
use machined_platform::Platform;
use machined_resources::{Resource, ResourceType};
use machined_runtime_core::{Controller, Input, InputKind, Output, ReconcileCtx};

use super::{ctl, NS};

pub struct HostnameController {
    platform: Arc<dyn Platform>,
}

impl HostnameController {
    pub fn new(platform: Arc<dyn Platform>) -> Self {
        Self { platform }
    }
}

#[async_trait]
impl Controller for HostnameController {
    fn name(&self) -> &str {
        "hostname"
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::HostnameSpec,
            kind: InputKind::Strong,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        Vec::new()
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        // Hostname has no meaningful teardown; apply for any Running spec.
        for obj in ctx.state.list(NS, ResourceType::HostnameSpec) {
            if let Resource::HostnameSpec(s) = &obj.spec {
                self.platform.set_hostname(&s.hostname).map_err(ctl)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_platform::FakePlatform;
    use machined_resources::{HostnameSpec, ResourceObject};
    use machined_runtime_core::{ReconcileCtx, State};

    #[tokio::test]
    async fn sets_hostname() {
        let platform = Arc::new(FakePlatform::new());
        let state = State::new();
        state
            .create(ResourceObject::new(
                NS,
                "hostname",
                Resource::HostnameSpec(HostnameSpec {
                    hostname: "node-1".into(),
                }),
            ))
            .unwrap();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = HostnameController::new(platform.clone());
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(
            platform.recorded.lock().unwrap().hostname.as_deref(),
            Some("node-1")
        );
    }
}
