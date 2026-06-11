//! Network resource specs (desired) and status (observed). Pure data.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

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

/// Error parsing an `AddrCidr` from `ip/prefix` text.
#[derive(Debug, PartialEq, Eq)]
pub struct AddrCidrParseError;

impl fmt::Display for AddrCidrParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid ip/prefix address")
    }
}

impl std::error::Error for AddrCidrParseError {}

impl FromStr for AddrCidr {
    type Err = AddrCidrParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (ip_s, prefix_s) = s.split_once('/').ok_or(AddrCidrParseError)?;
        let ip: IpAddr = ip_s.parse().map_err(|_| AddrCidrParseError)?;
        let prefix: u8 = prefix_s.parse().map_err(|_| AddrCidrParseError)?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(AddrCidrParseError);
        }
        Ok(AddrCidr { ip, prefix })
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

    #[test]
    fn addr_cidr_parses() {
        let a: AddrCidr = "192.168.1.10/24".parse().unwrap();
        assert_eq!(a.ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)));
        assert_eq!(a.prefix, 24);
        assert!("nope".parse::<AddrCidr>().is_err());
        assert!("192.168.1.10/x".parse::<AddrCidr>().is_err());
        assert!("192.168.1.10".parse::<AddrCidr>().is_err());
    }
}
