//! Real `NetworkBackend` over the kernel via the `rtnetlink` crate. Linux only.
//! Exercised by the netns integration test, not unit tests.

use std::net::IpAddr;

use async_trait::async_trait;
use futures::TryStreamExt;
use machined_resources::AddrCidr;
use rtnetlink::Handle;

use crate::{LinkState, NetlinkError, NetworkBackend, Result, RouteReq};

/// Real netlink backend. Construct with [`RtNetlink::new`].
pub struct RtNetlink {
    handle: Handle,
}

impl RtNetlink {
    /// Open a netlink connection and spawn its driver onto the tokio runtime.
    ///
    /// # Panics
    ///
    /// Must be called within a tokio runtime context: it `tokio::spawn`s the
    /// connection driver, which panics if no runtime is active.
    pub fn new() -> Result<Self> {
        let (connection, handle, _) =
            rtnetlink::new_connection().map_err(|e| NetlinkError::Netlink(e.to_string()))?;
        tokio::spawn(connection);
        Ok(Self { handle })
    }

    async fn link_index(&self, name: &str) -> Result<u32> {
        let mut links = self
            .handle
            .link()
            .get()
            .match_name(name.to_string())
            .execute();
        match links.try_next().await.map_err(net)? {
            Some(msg) => Ok(msg.header.index),
            None => Err(NetlinkError::LinkNotFound(name.to_string())),
        }
    }
}

fn net<E: std::fmt::Display>(e: E) -> NetlinkError {
    NetlinkError::Netlink(e.to_string())
}

#[async_trait]
impl NetworkBackend for RtNetlink {
    async fn list_links(&self) -> Result<Vec<LinkState>> {
        use netlink_packet_route::link::LinkAttribute;
        let mut out = Vec::new();
        let mut links = self.handle.link().get().execute();
        while let Some(msg) = links.try_next().await.map_err(net)? {
            let mut name = String::new();
            let mut mtu = 0u32;
            let mut mac = String::new();
            for attr in &msg.attributes {
                match attr {
                    LinkAttribute::IfName(n) => name = n.clone(),
                    LinkAttribute::Mtu(m) => mtu = *m,
                    LinkAttribute::Address(bytes) => {
                        mac = bytes
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(":");
                    }
                    _ => {}
                }
            }
            let up = msg
                .header
                .flags
                .contains(&netlink_packet_route::link::LinkFlag::Up);
            out.push(LinkState { name, up, mtu, mac });
        }
        Ok(out)
    }

    async fn set_link_up(&self, name: &str, up: bool) -> Result<()> {
        let index = self.link_index(name).await?;
        let req = self.handle.link().set(index);
        if up {
            req.up().execute().await.map_err(net)
        } else {
            req.down().execute().await.map_err(net)
        }
    }

    async fn set_mtu(&self, name: &str, mtu: u32) -> Result<()> {
        let index = self.link_index(name).await?;
        self.handle
            .link()
            .set(index)
            .mtu(mtu)
            .execute()
            .await
            .map_err(net)
    }

    async fn list_addresses(&self, link: &str) -> Result<Vec<AddrCidr>> {
        use netlink_packet_route::address::AddressAttribute;
        let index = self.link_index(link).await?;
        let mut out = Vec::new();
        let mut addrs = self
            .handle
            .address()
            .get()
            .set_link_index_filter(index)
            .execute();
        while let Some(msg) = addrs.try_next().await.map_err(net)? {
            let prefix = msg.header.prefix_len;
            for attr in &msg.attributes {
                if let AddressAttribute::Address(ip) = attr {
                    out.push(AddrCidr { ip: *ip, prefix });
                }
            }
        }
        Ok(out)
    }

    async fn add_address(&self, link: &str, addr: AddrCidr) -> Result<()> {
        let index = self.link_index(link).await?;
        self.handle
            .address()
            .add(index, addr.ip, addr.prefix)
            .execute()
            .await
            .map_err(net)
    }

    async fn del_address(&self, link: &str, addr: AddrCidr) -> Result<()> {
        use netlink_packet_route::address::AddressAttribute;
        let index = self.link_index(link).await?;
        let mut addrs = self
            .handle
            .address()
            .get()
            .set_link_index_filter(index)
            .execute();
        while let Some(msg) = addrs.try_next().await.map_err(net)? {
            let matches = msg.header.prefix_len == addr.prefix
                && msg
                    .attributes
                    .iter()
                    .any(|a| matches!(a, AddressAttribute::Address(ip) if *ip == addr.ip));
            if matches {
                self.handle
                    .address()
                    .del(msg)
                    .execute()
                    .await
                    .map_err(net)?;
                break;
            }
        }
        Ok(())
    }

    async fn add_route(&self, route: &RouteReq) -> Result<()> {
        let index = self.link_index(&route.link).await?;
        let add = self
            .handle
            .route()
            .add()
            .output_interface(index)
            .priority(route.metric);
        match (route.destination, route.gateway) {
            (Some(dst), gw) => match (dst.ip, gw) {
                (IpAddr::V4(d), Some(IpAddr::V4(g))) => add
                    .v4()
                    .destination_prefix(d, dst.prefix)
                    .gateway(g)
                    .execute()
                    .await
                    .map_err(net),
                (IpAddr::V4(d), None) => add
                    .v4()
                    .destination_prefix(d, dst.prefix)
                    .execute()
                    .await
                    .map_err(net),
                (IpAddr::V6(d), Some(IpAddr::V6(g))) => add
                    .v6()
                    .destination_prefix(d, dst.prefix)
                    .gateway(g)
                    .execute()
                    .await
                    .map_err(net),
                (IpAddr::V6(d), None) => add
                    .v6()
                    .destination_prefix(d, dst.prefix)
                    .execute()
                    .await
                    .map_err(net),
                _ => Err(NetlinkError::Netlink("mixed v4/v6 route".into())),
            },
            (None, Some(IpAddr::V4(g))) => add.v4().gateway(g).execute().await.map_err(net),
            (None, Some(IpAddr::V6(g))) => add.v6().gateway(g).execute().await.map_err(net),
            (None, None) => Err(NetlinkError::Netlink(
                "default route needs a gateway".into(),
            )),
        }
    }

    async fn del_route(&self, _route: &RouteReq) -> Result<()> {
        // Route deletion requires building a matching RouteMessage; for M2a the
        // controllers revert addresses/links but routes are torn down by link
        // teardown. A full del_route lands with M2b's richer route handling.
        // Returning Ok keeps revert idempotent.
        Ok(())
    }
}
