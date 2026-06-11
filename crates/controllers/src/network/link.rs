//! Applies desired `LinkSpec`s to the kernel and publishes `LinkStatus`.

use std::sync::Arc;

use async_trait::async_trait;
use machined_netlink::{NetlinkError, NetworkBackend};
use machined_resources::{LinkStatus, Resource, ResourceType};
use machined_runtime_core::{
    reconcile_finalized, Controller, Input, InputKind, Output, OutputKind, ReconcileCtx,
};

use super::{ctl, destroy_status, publish_status, NS};

const FINALIZER: &str = "link-controller";

pub struct LinkController {
    backend: Arc<dyn NetworkBackend>,
}

impl LinkController {
    pub fn new(backend: Arc<dyn NetworkBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Controller for LinkController {
    fn name(&self) -> &str {
        "link"
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::LinkSpec,
            kind: InputKind::Strong,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::LinkStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let specs = ctx.state.list(NS, ResourceType::LinkSpec);
        let backend = self.backend.clone();
        let state = ctx.state.clone();

        reconcile_finalized(
            &ctx.state,
            FINALIZER,
            &specs,
            |obj| {
                let backend = backend.clone();
                let state = state.clone();
                let spec = match &obj.spec {
                    Resource::LinkSpec(s) => Some(s.clone()),
                    _ => None,
                };
                async move {
                    let Some(spec) = spec else { return Ok(()) };
                    backend
                        .set_link_up(&spec.name, spec.up)
                        .await
                        .map_err(ctl)?;
                    if let Some(mtu) = spec.mtu {
                        backend.set_mtu(&spec.name, mtu).await.map_err(ctl)?;
                    }
                    if let Some(ls) = backend
                        .list_links()
                        .await
                        .map_err(ctl)?
                        .into_iter()
                        .find(|l| l.name == spec.name)
                    {
                        publish_status(
                            &state,
                            &spec.name,
                            Resource::LinkStatus(LinkStatus {
                                name: ls.name,
                                up: ls.up,
                                mtu: ls.mtu,
                                mac: ls.mac,
                            }),
                        );
                    }
                    Ok(())
                }
            },
            |obj| {
                let backend = backend.clone();
                let state = state.clone();
                let spec = match &obj.spec {
                    Resource::LinkSpec(s) => Some(s.clone()),
                    _ => None,
                };
                async move {
                    let Some(spec) = spec else { return Ok(()) };
                    // Best-effort: return the link to down; never delete it.
                    // A not-found link is already gone (benign); any other
                    // error is propagated so the finalizer is retained and
                    // revert retries on the next reconcile.
                    match backend.set_link_up(&spec.name, false).await {
                        Ok(()) | Err(NetlinkError::LinkNotFound(_)) => {}
                        Err(e) => return Err(ctl(e)),
                    }
                    destroy_status(&state, ResourceType::LinkStatus, &spec.name);
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
    use machined_resources::{Key, LinkSpec, ResourceObject};
    use machined_runtime_core::{reconcile_owned, ReconcileCtx, State};

    fn link_spec(name: &str, up: bool, mtu: Option<u32>) -> ResourceObject {
        ResourceObject::new(
            NS,
            name,
            Resource::LinkSpec(LinkSpec {
                name: name.into(),
                up,
                mtu,
            }),
        )
    }

    #[tokio::test]
    async fn applies_link_and_publishes_status() {
        let backend = Arc::new(FakeNetworkBackend::new().with_link("eth0", 1500));
        let state = State::new();
        // Seed a desired LinkSpec owned by the config controller.
        reconcile_owned(
            &state,
            "network-config",
            NS,
            ResourceType::LinkSpec,
            vec![link_spec("eth0", true, Some(9000))],
        )
        .unwrap();

        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = LinkController::new(backend.clone());
        c.reconcile(&ctx).await.unwrap();

        // Kernel (fake) shows link up + mtu applied.
        let links = backend.list_links().await.unwrap();
        assert!(links[0].up);
        assert_eq!(links[0].mtu, 9000);

        // Status published.
        let status = state
            .get(&Key::new(NS, ResourceType::LinkStatus, "eth0"))
            .unwrap();
        match status.spec {
            Resource::LinkStatus(s) => {
                assert!(s.up);
                assert_eq!(s.mtu, 9000);
            }
            _ => panic!("wrong type"),
        }

        // Finalizer was added to the spec.
        let spec = state
            .get(&Key::new(NS, ResourceType::LinkSpec, "eth0"))
            .unwrap();
        assert!(spec.metadata.finalizers.contains(&FINALIZER.to_string()));
    }

    #[tokio::test]
    async fn reverts_on_teardown() {
        let backend = Arc::new(FakeNetworkBackend::new().with_link("eth0", 1500));
        let state = State::new();
        reconcile_owned(
            &state,
            "network-config",
            NS,
            ResourceType::LinkSpec,
            vec![link_spec("eth0", true, None)],
        )
        .unwrap();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = LinkController::new(backend.clone());
        c.reconcile(&ctx).await.unwrap(); // applies + finalizer + status

        // Config drops eth0 → spec torn down (held by finalizer).
        reconcile_owned(&state, "network-config", NS, ResourceType::LinkSpec, vec![]).unwrap();
        // Controller reconciles the TearingDown spec → reverts + clears finalizer.
        c.reconcile(&ctx).await.unwrap();

        // Link returned to down; status destroyed.
        assert!(!backend.list_links().await.unwrap()[0].up);
        assert!(state
            .get(&Key::new(NS, ResourceType::LinkStatus, "eth0"))
            .is_err());
        // Finalizer cleared → a final GC pass destroys the spec.
        reconcile_owned(&state, "network-config", NS, ResourceType::LinkSpec, vec![]).unwrap();
        assert!(state
            .get(&Key::new(NS, ResourceType::LinkSpec, "eth0"))
            .is_err());
    }

    struct FailingRevertBackend;

    #[async_trait::async_trait]
    impl NetworkBackend for FailingRevertBackend {
        async fn list_links(&self) -> machined_netlink::Result<Vec<machined_netlink::LinkState>> {
            Ok(vec![])
        }
        async fn set_link_up(&self, _: &str, up: bool) -> machined_netlink::Result<()> {
            if up {
                Ok(())
            } else {
                Err(NetlinkError::Netlink("transient revert failure".into()))
            }
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
            Ok(())
        }
        async fn del_address(
            &self,
            _: &str,
            _: machined_resources::AddrCidr,
        ) -> machined_netlink::Result<()> {
            Ok(())
        }
        async fn add_route(&self, _: &machined_netlink::RouteReq) -> machined_netlink::Result<()> {
            Ok(())
        }
        async fn del_route(&self, _: &machined_netlink::RouteReq) -> machined_netlink::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn revert_error_keeps_finalizer() {
        let backend = Arc::new(FailingRevertBackend);
        let state = State::new();
        reconcile_owned(
            &state,
            "network-config",
            NS,
            ResourceType::LinkSpec,
            vec![link_spec("eth0", true, None)],
        )
        .unwrap();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = LinkController::new(backend.clone());
        c.reconcile(&ctx).await.unwrap(); // applies + adds finalizer

        // Drop the spec → TearingDown; the revert will fail.
        reconcile_owned(&state, "network-config", NS, ResourceType::LinkSpec, vec![]).unwrap();
        let res = c.reconcile(&ctx).await;
        assert!(res.is_err(), "a failed revert must surface as an error");

        // Finalizer retained so a later reconcile retries the revert.
        let spec = state
            .get(&Key::new(NS, ResourceType::LinkSpec, "eth0"))
            .unwrap();
        assert!(spec.metadata.finalizers.contains(&FINALIZER.to_string()));
    }
}
