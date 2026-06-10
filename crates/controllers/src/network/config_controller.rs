//! Translates the typed `network` config into owned desired specs.

use async_trait::async_trait;
use machined_config::Provider;
use machined_resources::{
    AddrCidr, AddressSpec, HostnameSpec, LinkSpec, ResolverSpec, Resource, ResourceObject,
    ResourceType, RouteSpec,
};
use machined_runtime_core::{reconcile_owned, Controller, Input, Output, OutputKind, ReconcileCtx};
use tracing::warn;

use super::NS;

const OWNER: &str = "network-config";

/// Reads the static `network` config and produces the desired Link/Address/
/// Route/Hostname/Resolver specs, garbage-collecting any that leave the config.
pub struct NetworkConfigController {
    provider: Provider,
}

impl NetworkConfigController {
    pub fn new(provider: Provider) -> Self {
        Self { provider }
    }
}

fn obj(id: &str, spec: Resource) -> ResourceObject {
    ResourceObject::new(NS, id, spec)
}

#[async_trait]
impl Controller for NetworkConfigController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        // Static config in M2a: the single startup reconcile produces the specs.
        Vec::new()
    }

    fn outputs(&self) -> Vec<Output> {
        [
            ResourceType::LinkSpec,
            ResourceType::AddressSpec,
            ResourceType::RouteSpec,
            ResourceType::HostnameSpec,
            ResourceType::ResolverSpec,
        ]
        .into_iter()
        .map(|typ| Output {
            typ,
            kind: OutputKind::Exclusive,
        })
        .collect()
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let net = self.provider.network();

        let mut links = Vec::new();
        let mut addresses = Vec::new();
        let mut routes = Vec::new();

        for iface in &net.interfaces {
            links.push(obj(
                &iface.name,
                Resource::LinkSpec(LinkSpec {
                    name: iface.name.clone(),
                    up: iface.up,
                    mtu: iface.mtu,
                }),
            ));

            for addr_s in &iface.addresses {
                match addr_s.parse::<AddrCidr>() {
                    Ok(address) => {
                        let id = format!("{}/{}", iface.name, address);
                        addresses.push(obj(
                            &id,
                            Resource::AddressSpec(AddressSpec {
                                link: iface.name.clone(),
                                address,
                            }),
                        ));
                    }
                    Err(_) => {
                        warn!(iface = %iface.name, addr = %addr_s, "invalid address, skipping")
                    }
                }
            }

            for r in &iface.routes {
                let destination = match r.to.as_deref() {
                    None | Some("0.0.0.0/0") | Some("::/0") => None,
                    Some(cidr) => match cidr.parse::<AddrCidr>() {
                        Ok(c) => Some(c),
                        Err(_) => {
                            warn!(iface = %iface.name, route = %cidr, "invalid route dest, skipping");
                            continue;
                        }
                    },
                };
                let dest_label = destination
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "default".to_string());
                let id = format!("{}/{}/{}", iface.name, dest_label, r.via);
                routes.push(obj(
                    &id,
                    Resource::RouteSpec(RouteSpec {
                        destination,
                        gateway: Some(r.via),
                        link: iface.name.clone(),
                        metric: r.metric.unwrap_or(0),
                    }),
                ));
            }
        }

        // Hostname comes from machine.hostname (not the network block).
        let mut hostnames = Vec::new();
        if let Some(h) = self.provider.hostname() {
            hostnames.push(obj(
                "hostname",
                Resource::HostnameSpec(HostnameSpec {
                    hostname: h.to_string(),
                }),
            ));
        }

        let mut resolvers = Vec::new();
        if !net.nameservers.is_empty() {
            resolvers.push(obj(
                "resolver",
                Resource::ResolverSpec(ResolverSpec {
                    nameservers: net.nameservers.clone(),
                    search: net.search.clone(),
                }),
            ));
        }

        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::LinkSpec, links)?;
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::AddressSpec, addresses)?;
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::RouteSpec, routes)?;
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::HostnameSpec, hostnames)?;
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::ResolverSpec, resolvers)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::{
        InterfaceConfig, MachineConfig, MachineSection, NetworkSection, RouteConfig,
    };
    use machined_resources::{Key, Resource};
    use machined_runtime_core::{ReconcileCtx, State};
    use std::net::{IpAddr, Ipv4Addr};

    fn provider() -> Provider {
        Provider::new(MachineConfig {
            machine: MachineSection {
                hostname: Some("node-1".into()),
                sysctls: vec![],
                services: vec![],
                network: NetworkSection {
                    interfaces: vec![InterfaceConfig {
                        name: "eth0".into(),
                        up: true,
                        mtu: Some(1500),
                        addresses: vec!["192.168.1.10/24".into(), "bad-addr".into()],
                        routes: vec![RouteConfig {
                            to: Some("0.0.0.0/0".into()),
                            via: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
                            metric: Some(100),
                        }],
                    }],
                    nameservers: vec![IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))],
                    search: vec!["example.com".into()],
                },
            },
        })
    }

    #[tokio::test]
    async fn produces_specs_from_config() {
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = NetworkConfigController::new(provider());
        c.reconcile(&ctx).await.unwrap();

        // One link, one valid address (bad-addr skipped), one route, hostname, resolver.
        assert_eq!(state.list(NS, ResourceType::LinkSpec).len(), 1);
        assert_eq!(state.list(NS, ResourceType::AddressSpec).len(), 1);
        assert_eq!(state.list(NS, ResourceType::RouteSpec).len(), 1);
        assert_eq!(state.list(NS, ResourceType::HostnameSpec).len(), 1);
        assert_eq!(state.list(NS, ResourceType::ResolverSpec).len(), 1);

        // The route is a default route (destination None).
        let routes = state.list(NS, ResourceType::RouteSpec);
        match &routes[0].spec {
            Resource::RouteSpec(r) => {
                assert!(r.destination.is_none());
                assert_eq!(r.metric, 100);
            }
            _ => panic!("wrong type"),
        }

        // Owned by the config controller.
        let link = state
            .get(&Key::new(NS, ResourceType::LinkSpec, "eth0"))
            .unwrap();
        assert_eq!(link.metadata.owner.as_deref(), Some("network-config"));
    }

    #[tokio::test]
    async fn gcs_specs_when_interface_removed() {
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = NetworkConfigController::new(provider());
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(state.list(NS, ResourceType::LinkSpec).len(), 1);

        // Reconcile with empty config → specs GC'd (no finalizers yet).
        let mut empty = NetworkConfigController::new(Provider::new(MachineConfig::default()));
        empty.reconcile(&ctx).await.unwrap();
        assert_eq!(state.list(NS, ResourceType::LinkSpec).len(), 0);
        assert_eq!(state.list(NS, ResourceType::AddressSpec).len(), 0);
    }
}
