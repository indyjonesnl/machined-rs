//! Network resource specs (desired) and status (observed). Pure data.

use std::fmt;
use std::net::IpAddr;

/// An IP address with a prefix length, e.g. `192.168.1.10/24`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AddrCidr {
    pub ip: IpAddr,
    pub prefix: u8,
}

impl AddrCidr {
    pub fn new(ip: IpAddr, prefix: u8) -> Self {
        Self { ip, prefix }
    }
}

impl fmt::Display for AddrCidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.ip, self.prefix)
    }
}

// --- Desired specs (derived from config, controller-owned) ---

/// Desired admin state of a network link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSpec {
    pub name: String,
    pub up: bool,
    pub mtu: Option<u32>,
}

/// Desired address assignment on a link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressSpec {
    pub link: String,
    pub address: AddrCidr,
}

/// Desired route. `destination == None` means the default route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSpec {
    pub destination: Option<AddrCidr>,
    pub gateway: Option<IpAddr>,
    pub link: String,
    pub metric: u32,
}

/// Desired hostname.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostnameSpec {
    pub hostname: String,
}

/// Desired DNS resolver configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolverSpec {
    pub nameservers: Vec<IpAddr>,
    pub search: Vec<String>,
}

// --- Observed status (published by controllers after applying / reading back) ---

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkStatus {
    pub name: String,
    pub up: bool,
    pub mtu: u32,
    pub mac: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressStatus {
    pub link: String,
    pub address: AddrCidr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteStatus {
    pub destination: Option<AddrCidr>,
    pub gateway: Option<IpAddr>,
    pub link: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn addr_cidr_display() {
        let a = AddrCidr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), 24);
        assert_eq!(a.to_string(), "192.168.1.10/24");
    }
}
