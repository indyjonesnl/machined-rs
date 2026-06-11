//! Applies desired `AddressSpec`s to the kernel and publishes `AddressStatus`.

use std::sync::Arc;

use async_trait::async_trait;
use machined_netlink::{NetlinkError, NetworkBackend};
use machined_resources::{AddressStatus, Resource, ResourceType};
use machined_runtime_core::{
    reconcile_finalized, Controller, Input, InputKind, Output, OutputKind, ReconcileCtx,
};

use super::{ctl, destroy_status, publish_status, NS};

const FINALIZER: &str = "address-controller";

pub struct AddressController {
    backend: Arc<dyn NetworkBackend>,
}

impl AddressController {
    pub fn new(backend: Arc<dyn NetworkBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Controller for AddressController {
    fn name(&self) -> &str {
        "address"
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::AddressSpec,
            kind: InputKind::Strong,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::AddressStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let specs = ctx.state.list(NS, ResourceType::AddressSpec);
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
                    Resource::AddressSpec(s) => (Some(s.clone()), obj.metadata.id.clone()),
                    _ => (None, String::new()),
                };
                async move {
                    let Some(spec) = spec else { return Ok(()) };
                    backend
                        .add_address(&spec.link, spec.address)
                        .await
                        .map_err(ctl)?;
                    publish_status(
                        &state,
                        &id,
                        Resource::AddressStatus(AddressStatus {
                            link: spec.link,
                            address: spec.address,
                        }),
                    );
                    Ok(())
                }
            },
            |obj| {
                let backend = backend.clone();
                let state = state.clone();
                let (spec, id) = match &obj.spec {
                    Resource::AddressSpec(s) => (Some(s.clone()), obj.metadata.id.clone()),
                    _ => (None, String::new()),
                };
                async move {
                    let Some(spec) = spec else { return Ok(()) };
                    match backend.del_address(&spec.link, spec.address).await {
                        Ok(()) | Err(NetlinkError::LinkNotFound(_)) => {}
                        Err(e) => return Err(ctl(e)),
                    }
                    destroy_status(&state, ResourceType::AddressStatus, &id);
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
    use machined_resources::{AddrCidr, AddressSpec, Key, ResourceObject};
    use machined_runtime_core::{reconcile_owned, ReconcileCtx, State};
    use std::net::{IpAddr, Ipv4Addr};

    fn addr() -> AddrCidr {
        AddrCidr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 24)
    }

    fn addr_spec() -> ResourceObject {
        ResourceObject::new(
            NS,
            "eth0/192.168.1.10/24",
            Resource::AddressSpec(AddressSpec {
                link: "eth0".into(),
                address: addr(),
            }),
        )
    }

    #[tokio::test]
    async fn applies_address_and_reverts() {
        let backend = Arc::new(FakeNetworkBackend::new().with_link("eth0", 1500));
        let state = State::new();
        reconcile_owned(
            &state,
            "network-config",
            NS,
            ResourceType::AddressSpec,
            vec![addr_spec()],
        )
        .unwrap();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = AddressController::new(backend.clone());
        c.reconcile(&ctx).await.unwrap();

        assert_eq!(backend.list_addresses("eth0").await.unwrap(), vec![addr()]);
        assert!(state
            .get(&Key::new(
                NS,
                ResourceType::AddressStatus,
                "eth0/192.168.1.10/24"
            ))
            .is_ok());

        // Teardown removes the address.
        reconcile_owned(
            &state,
            "network-config",
            NS,
            ResourceType::AddressSpec,
            vec![],
        )
        .unwrap();
        c.reconcile(&ctx).await.unwrap();
        assert!(backend.list_addresses("eth0").await.unwrap().is_empty());
    }
}
