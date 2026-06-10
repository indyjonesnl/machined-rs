//! In-memory `NetworkBackend` that simulates kernel state, for root-free tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use machined_resources::AddrCidr;

use crate::{LinkState, NetlinkError, NetworkBackend, Result, RouteReq};

#[derive(Default)]
struct FakeState {
    links: BTreeMap<String, LinkState>,
    addrs: BTreeMap<String, Vec<AddrCidr>>,
    routes: Vec<RouteReq>,
}

/// A `NetworkBackend` backed by in-memory maps. Operations mutate the simulated
/// kernel so `list_*` reflect prior `set_*`/`add_*`/`del_*` calls.
#[derive(Default)]
pub struct FakeNetworkBackend {
    state: Mutex<FakeState>,
}

impl FakeNetworkBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a link in the "down" admin state with the given MTU (as the kernel
    /// would present an existing NIC before configuration).
    pub fn with_link(self, name: &str, mtu: u32) -> Self {
        self.state.lock().unwrap().links.insert(
            name.to_string(),
            LinkState {
                name: name.to_string(),
                up: false,
                mtu,
                mac: "00:00:00:00:00:00".to_string(),
            },
        );
        self
    }

    /// Inspect the simulated routes. The `NetworkBackend` trait has no
    /// `list_routes`, so this fake-only accessor lets tests observe applied
    /// routes.
    pub fn routes(&self) -> Vec<RouteReq> {
        self.state.lock().unwrap().routes.clone()
    }
}

#[async_trait]
impl NetworkBackend for FakeNetworkBackend {
    async fn list_links(&self) -> Result<Vec<LinkState>> {
        Ok(self.state.lock().unwrap().links.values().cloned().collect())
    }

    async fn set_link_up(&self, name: &str, up: bool) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let link = st
            .links
            .get_mut(name)
            .ok_or_else(|| NetlinkError::LinkNotFound(name.to_string()))?;
        link.up = up;
        Ok(())
    }

    async fn set_mtu(&self, name: &str, mtu: u32) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let link = st
            .links
            .get_mut(name)
            .ok_or_else(|| NetlinkError::LinkNotFound(name.to_string()))?;
        link.mtu = mtu;
        Ok(())
    }

    async fn list_addresses(&self, link: &str) -> Result<Vec<AddrCidr>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .addrs
            .get(link)
            .cloned()
            .unwrap_or_default())
    }

    async fn add_address(&self, link: &str, addr: AddrCidr) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if !st.links.contains_key(link) {
            return Err(NetlinkError::LinkNotFound(link.to_string()));
        }
        let list = st.addrs.entry(link.to_string()).or_default();
        if !list.contains(&addr) {
            list.push(addr);
        }
        Ok(())
    }

    async fn del_address(&self, link: &str, addr: AddrCidr) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if let Some(list) = st.addrs.get_mut(link) {
            list.retain(|a| *a != addr);
        }
        Ok(())
    }

    async fn add_route(&self, route: &RouteReq) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        if !st.routes.contains(route) {
            st.routes.push(route.clone());
        }
        Ok(())
    }

    async fn del_route(&self, route: &RouteReq) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.routes.retain(|r| r != route);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn cidr(d: u8, p: u8) -> AddrCidr {
        AddrCidr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, d)), p)
    }

    #[tokio::test]
    async fn link_up_and_mtu() {
        let be = FakeNetworkBackend::new().with_link("eth0", 1500);
        be.set_link_up("eth0", true).await.unwrap();
        be.set_mtu("eth0", 9000).await.unwrap();
        let links = be.list_links().await.unwrap();
        assert_eq!(links.len(), 1);
        assert!(links[0].up);
        assert_eq!(links[0].mtu, 9000);
    }

    #[tokio::test]
    async fn unknown_link_errors() {
        let be = FakeNetworkBackend::new();
        assert!(matches!(
            be.set_link_up("ghost", true).await,
            Err(NetlinkError::LinkNotFound(_))
        ));
    }

    #[tokio::test]
    async fn address_add_del_is_idempotent() {
        let be = FakeNetworkBackend::new().with_link("eth0", 1500);
        be.add_address("eth0", cidr(10, 24)).await.unwrap();
        be.add_address("eth0", cidr(10, 24)).await.unwrap(); // dup is no-op
        assert_eq!(be.list_addresses("eth0").await.unwrap(), vec![cidr(10, 24)]);
        be.del_address("eth0", cidr(10, 24)).await.unwrap();
        assert!(be.list_addresses("eth0").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_address_unknown_link_errors() {
        let be = FakeNetworkBackend::new();
        assert!(matches!(
            be.add_address("ghost", cidr(10, 24)).await,
            Err(NetlinkError::LinkNotFound(_))
        ));
    }

    #[tokio::test]
    async fn route_add_del_is_idempotent() {
        let be = FakeNetworkBackend::new();
        let r = RouteReq {
            destination: Some(AddrCidr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 5, 0)), 24)),
            gateway: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
            link: "eth0".into(),
            metric: 100,
        };
        be.add_route(&r).await.unwrap();
        be.add_route(&r).await.unwrap(); // dup is a no-op
        assert_eq!(be.routes().len(), 1);
        be.del_route(&r).await.unwrap();
        be.del_route(&r).await.unwrap(); // del-again is a no-op
        assert!(be.routes().is_empty());
    }
}
