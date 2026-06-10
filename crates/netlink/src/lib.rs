//! Network configuration backend: a `NetworkBackend` trait abstracting netlink
//! link/address/route operations, with an in-memory fake and (on Linux) a real
//! `rtnetlink` implementation.

pub mod fake;
#[cfg(target_os = "linux")]
pub mod rtnetlink_backend;

use std::net::IpAddr;

use async_trait::async_trait;
use machined_resources::AddrCidr;

pub use fake::FakeNetworkBackend;
#[cfg(target_os = "linux")]
pub use rtnetlink_backend::RtNetlink;

#[derive(thiserror::Error, Debug)]
pub enum NetlinkError {
    #[error("link not found: {0}")]
    LinkNotFound(String),
    #[error("netlink error: {0}")]
    Netlink(String),
}

pub type Result<T> = std::result::Result<T, NetlinkError>;

/// Observed state of a link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkState {
    pub name: String,
    pub up: bool,
    pub mtu: u32,
    pub mac: String,
}

/// A route to add or delete. `destination == None` is the default route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteReq {
    pub destination: Option<AddrCidr>,
    pub gateway: Option<IpAddr>,
    pub link: String,
    pub metric: u32,
}

/// Abstraction over kernel network configuration. All methods are idempotent
/// from the caller's perspective: adding an existing address or setting an
/// already-correct link state is a successful no-op.
#[async_trait]
pub trait NetworkBackend: Send + Sync {
    async fn list_links(&self) -> Result<Vec<LinkState>>;
    async fn set_link_up(&self, name: &str, up: bool) -> Result<()>;
    async fn set_mtu(&self, name: &str, mtu: u32) -> Result<()>;
    async fn list_addresses(&self, link: &str) -> Result<Vec<AddrCidr>>;
    async fn add_address(&self, link: &str, addr: AddrCidr) -> Result<()>;
    async fn del_address(&self, link: &str, addr: AddrCidr) -> Result<()>;
    async fn add_route(&self, route: &RouteReq) -> Result<()>;
    async fn del_route(&self, route: &RouteReq) -> Result<()>;
}
