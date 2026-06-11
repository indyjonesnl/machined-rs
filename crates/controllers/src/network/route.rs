//! Applies desired `RouteSpec`s to the kernel and publishes `RouteStatus`.

use std::sync::Arc;

use async_trait::async_trait;
use machined_netlink::{NetlinkError, NetworkBackend, RouteReq};
use machined_resources::{Resource, ResourceType, RouteStatus};
use machined_runtime_core::{
    reconcile_finalized, Controller, Input, InputKind, Output, OutputKind, ReconcileCtx,
};

use super::{ctl, destroy_status, publish_status, NS};

const FINALIZER: &str = "route-controller";

pub struct RouteController {
    backend: Arc<dyn NetworkBackend>,
}

impl RouteController {
    pub fn new(backend: Arc<dyn NetworkBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Controller for RouteController {
    fn name(&self) -> &str {
        "route"
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::RouteSpec,
            kind: InputKind::Strong,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::RouteStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let specs = ctx.state.list(NS, ResourceType::RouteSpec);
        let backend = self.backend.clone();
        let state = ctx.state.clone();

        reconcile_finalized(
            &ctx.state,
            FINALIZER,
            &specs,
            |obj| {
                let backend = backend.clone();
                let state = state.clone();
                let (spec, id) = match &obj.spec {
                    Resource::RouteSpec(s) => (Some(s.clone()), obj.metadata.id.clone()),
                    _ => (None, String::new()),
                };
                async move {
                    let Some(spec) = spec else { return Ok(()) };
                    let req = RouteReq {
                        destination: spec.destination,
                        gateway: spec.gateway,
                        link: spec.link.clone(),
                        metric: spec.metric,
                    };
                    backend.add_route(&req).await.map_err(ctl)?;
                    publish_status(
                        &state,
                        &id,
                        Resource::RouteStatus(RouteStatus {
                            destination: spec.destination,
                            gateway: spec.gateway,
                            link: spec.link,
                        }),
                    );
                    Ok(())
                }
            },
            |obj| {
                let backend = backend.clone();
                let state = state.clone();
                let (spec, id) = match &obj.spec {
                    Resource::RouteSpec(s) => (Some(s.clone()), obj.metadata.id.clone()),
                    _ => (None, String::new()),
                };
                async move {
                    let Some(spec) = spec else { return Ok(()) };
                    let req = RouteReq {
                        destination: spec.destination,
                        gateway: spec.gateway,
                        link: spec.link,
                        metric: spec.metric,
                    };
                    // not-found is benign; other errors retain the finalizer.
                    match backend.del_route(&req).await {
                        Ok(()) | Err(NetlinkError::LinkNotFound(_)) => {}
                        Err(e) => return Err(ctl(e)),
                    }
                    destroy_status(&state, ResourceType::RouteStatus, &id);
                    Ok(())
                }
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_netlink::FakeNetworkBackend;
    use machined_resources::{Key, ResourceObject, RouteSpec};
    use machined_runtime_core::{reconcile_owned, ReconcileCtx, State};
    use std::net::{IpAddr, Ipv4Addr};

    fn route_spec() -> ResourceObject {
        ResourceObject::new(
            NS,
            "eth0/default/192.168.1.1",
            Resource::RouteSpec(RouteSpec {
                destination: None,
                gateway: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
                link: "eth0".into(),
                metric: 100,
            }),
        )
    }

    #[tokio::test]
    async fn applies_route_and_publishes_status() {
        let backend = Arc::new(FakeNetworkBackend::new().with_link("eth0", 1500));
        let state = State::new();
        reconcile_owned(
            &state,
            "network-config",
            NS,
            ResourceType::RouteSpec,
            vec![route_spec()],
        )
        .unwrap();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = RouteController::new(backend.clone());
        c.reconcile(&ctx).await.unwrap();

        assert_eq!(backend.routes().len(), 1);
        assert!(state
            .get(&Key::new(
                NS,
                ResourceType::RouteStatus,
                "eth0/default/192.168.1.1"
            ))
            .is_ok());
    }
}
