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
        vec![
            Input {
                namespace: NS.to_string(),
                typ: ResourceType::RouteSpec,
                kind: InputKind::Strong,
            },
            // Watch address status (Weak: no ownership) so that when an address
            // lands we reconcile again. A gateway route fails with ENETUNREACH
            // until its on-link address exists; this is the retry chain that
            // lets the route converge once the AddressController publishes.
            Input {
                namespace: NS.to_string(),
                typ: ResourceType::AddressStatus,
                kind: InputKind::Weak,
            },
        ]
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

    /// RouteController must declare AddressStatus as an input so the runtime
    /// re-reconciles it when an address lands (the ENETUNREACH retry chain).
    #[test]
    fn route_watches_address_status() {
        let c = RouteController::new(Arc::new(FakeNetworkBackend::new()));
        let inputs = c.inputs();
        assert!(
            inputs
                .iter()
                .any(|i| i.typ == ResourceType::AddressStatus && i.namespace == NS),
            "route must watch AddressStatus to retry once the address exists"
        );
    }

    /// A backend whose `add_route` fails with ENETUNREACH until an on-link
    /// address has been added — modelling the kernel rejecting a gateway route
    /// whose next hop isn't yet reachable.
    #[derive(Default)]
    struct GatedRouteBackend {
        has_address: std::sync::atomic::AtomicBool,
        routes: std::sync::Mutex<Vec<RouteReq>>,
    }

    #[async_trait]
    impl NetworkBackend for GatedRouteBackend {
        async fn list_links(&self) -> machined_netlink::Result<Vec<machined_netlink::LinkState>> {
            Ok(vec![])
        }
        async fn set_link_up(&self, _: &str, _: bool) -> machined_netlink::Result<()> {
            Ok(())
        }
        async fn set_mtu(&self, _: &str, _: u32) -> machined_netlink::Result<()> {
            Ok(())
        }
        async fn list_addresses(
            &self,
            _: &str,
        ) -> machined_netlink::Result<Vec<machined_resources::AddrCidr>> {
            Ok(vec![])
        }
        async fn add_address(
            &self,
            _: &str,
            _: machined_resources::AddrCidr,
        ) -> machined_netlink::Result<()> {
            self.has_address
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn del_address(
            &self,
            _: &str,
            _: machined_resources::AddrCidr,
        ) -> machined_netlink::Result<()> {
            Ok(())
        }
        async fn add_route(&self, route: &RouteReq) -> machined_netlink::Result<()> {
            if !self.has_address.load(std::sync::atomic::Ordering::SeqCst) {
                // errno 101 = ENETUNREACH; transient, retried on next event.
                return Err(NetlinkError::Netlink("Network unreachable".into()));
            }
            self.routes.lock().unwrap().push(route.clone());
            Ok(())
        }
        async fn del_route(&self, _: &RouteReq) -> machined_netlink::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn route_converges_after_address_lands() {
        let backend = Arc::new(GatedRouteBackend::default());
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

        // First pass: no address yet → ENETUNREACH propagates as an error and
        // no RouteStatus is published. The finalizer is retained for retry.
        assert!(c.reconcile(&ctx).await.is_err());
        assert!(state
            .get(&Key::new(
                NS,
                ResourceType::RouteStatus,
                "eth0/default/192.168.1.1"
            ))
            .is_err());

        // The address lands (as the AddressController would do), waking a retry.
        backend
            .add_address(
                "eth0",
                machined_resources::AddrCidr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 24),
            )
            .await
            .unwrap();

        // Second reconcile converges.
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(backend.routes.lock().unwrap().len(), 1);
        assert!(state
            .get(&Key::new(
                NS,
                ResourceType::RouteStatus,
                "eth0/default/192.168.1.1"
            ))
            .is_ok());
    }
}
