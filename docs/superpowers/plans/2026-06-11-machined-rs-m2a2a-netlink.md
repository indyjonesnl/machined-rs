# machined-rs M2a-2a — netlink crate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M2a-1 complete on branch `spec/machined-rs-m2a-network`. Continue on that branch.

**Goal:** Add a `netlink` crate exposing a `NetworkBackend` trait (link/address/route CRUD + read-back) with a `FakeNetworkBackend` for root-free unit tests and an `RtNetlink` real implementation, plus an `AddrCidr` parser. This is the I/O layer the M2a-2b controllers will drive.

**Architecture:** Same trait/real/fake pattern as the `platform` crate. The async `NetworkBackend` trait isolates controllers from netlink. `FakeNetworkBackend` simulates kernel state in memory (so controller logic is fully unit-testable). `RtNetlink` (Linux-only) talks to the kernel via the `rtnetlink` crate and is validated by a network-namespace integration test, not unit tests.

**Tech Stack:** `rtnetlink` (real backend), `futures` (its streams), `async-trait`, `tokio`, `thiserror`. `AddrCidr` parsing uses `std::net`.

---

## File Structure

```
crates/resources/src/network.rs    # MODIFY: impl FromStr for AddrCidr
crates/netlink/
├── Cargo.toml                      # NEW
└── src/
    ├── lib.rs                      # NEW: NetworkBackend trait + LinkState/RouteReq + error + re-exports
    ├── fake.rs                     # NEW: FakeNetworkBackend (in-memory kernel sim)
    └── rtnetlink_backend.rs        # NEW (Linux cfg): RtNetlink real impl
crates/netlink/tests/
└── netns.rs                        # NEW: privileged netns integration test (ignored by default)
```

---

## Task 1: `AddrCidr` parsing in `resources`

**Files:**
- Modify: `crates/resources/src/network.rs`

- [ ] **Step 1: Write the failing test**

In `crates/resources/src/network.rs`, add to the `tests` module:

```rust
    #[test]
    fn addr_cidr_parses() {
        let a: AddrCidr = "192.168.1.10/24".parse().unwrap();
        assert_eq!(a.ip, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)));
        assert_eq!(a.prefix, 24);
        assert!("nope".parse::<AddrCidr>().is_err());
        assert!("192.168.1.10/x".parse::<AddrCidr>().is_err());
        assert!("192.168.1.10".parse::<AddrCidr>().is_err());
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-resources addr_cidr_parses`
Expected: FAIL — `FromStr` not implemented (compile error).

- [ ] **Step 3: Implement FromStr**

In `crates/resources/src/network.rs`, add the import at the top (the file already has `use std::net::IpAddr;` and `use std::fmt;`):

```rust
use std::str::FromStr;
```

And add, after the `impl fmt::Display for AddrCidr` block:

```rust
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
```

- [ ] **Step 4: Re-export the error type**

In `crates/resources/src/lib.rs`, add `AddrCidrParseError` to the `network` re-export list:

```rust
pub use network::{
    AddrCidr, AddrCidrParseError, AddressSpec, AddressStatus, HostnameSpec, LinkSpec, LinkStatus,
    ResolverSpec, RouteSpec, RouteStatus,
};
```

- [ ] **Step 5: Run tests + clippy**

Run: `cargo test -p machined-resources`
Expected: PASS — including `addr_cidr_parses`.

Run: `cargo clippy -p machined-resources --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/resources
git commit -m "feat(resources): parse AddrCidr from ip/prefix text"
```

---

## Task 2: `netlink` crate — trait + types + FakeNetworkBackend

**Files:**
- Modify: `Cargo.toml` (workspace members + deps)
- Create: `crates/netlink/Cargo.toml`
- Create: `crates/netlink/src/lib.rs`
- Create: `crates/netlink/src/fake.rs`

- [ ] **Step 1: Add workspace member + deps**

In the root `Cargo.toml`, add `"crates/netlink"` to `members`. Add to `[workspace.dependencies]`:

```toml
rtnetlink = "0.14"
futures = "0.3"

machined-netlink = { path = "crates/netlink" }
```

- [ ] **Step 2: Create the netlink crate manifest**

Create `crates/netlink/Cargo.toml`:

```toml
[package]
name = "machined-netlink"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
machined-resources.workspace = true
async-trait.workspace = true
thiserror.workspace = true
tracing.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
rtnetlink.workspace = true
futures.workspace = true
tokio.workspace = true

[dev-dependencies]
tokio = { workspace = true }
```

- [ ] **Step 3: Write the failing fake-backend tests**

Create `crates/netlink/src/fake.rs`:

```rust
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
}
```

- [ ] **Step 4: Write the crate root (trait + types)**

Create `crates/netlink/src/lib.rs`:

```rust
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
```

- [ ] **Step 5: Temporarily stub the linux backend so the crate compiles**

So Task 2 can build/test without the real impl, create a placeholder `crates/netlink/src/rtnetlink_backend.rs`:

```rust
// Real implementation lands in Task 3.
```

And temporarily comment the `rtnetlink_backend` module + `RtNetlink` re-export in `lib.rs`:

```rust
pub mod fake;
// #[cfg(target_os = "linux")] pub mod rtnetlink_backend;  // Task 3
```

```rust
pub use fake::FakeNetworkBackend;
// #[cfg(target_os = "linux")] pub use rtnetlink_backend::RtNetlink;  // Task 3
```

- [ ] **Step 6: Run the fake tests + clippy**

Run: `cargo test -p machined-netlink`
Expected: PASS — the four fake tests pass.

Run: `cargo clippy -p machined-netlink --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add Cargo.toml Cargo.lock crates/netlink
git commit -m "feat(netlink): NetworkBackend trait + in-memory fake"
```

> **Review follow-up (applied):** the fake also gets a `pub fn routes(&self) -> Vec<RouteReq>`
> inspection accessor (the trait has no `list_routes`) and a `route_add_del_is_idempotent` test
> covering `add_route` dedup + `del_route` retain — the route path otherwise has the same idempotency
> contract as addresses but no coverage. M2a-2b's RouteController tests use `routes()` to observe
> applied routes against the fake.

---

## Task 3: `RtNetlink` real backend + netns integration test

> **SPIKE NOTE — read first.** The `rtnetlink` crate's builder API has shifted across
> versions. The code below targets `rtnetlink` 0.14 and is **best-effort**: if it does not
> compile against the installed version, that is expected spike work — adjust the calls to match
> the actual API (the method *names* may differ; the operations — get link by name, set up/mtu,
> add/del address, add/del route — are stable concepts). **Report exactly what you changed.** The
> netns integration test (Step 4) is the real acceptance criterion. Do not change the
> `NetworkBackend` trait signature to work around the API — adapt inside `RtNetlink`. If the trait
> itself cannot be satisfied, STOP and report BLOCKED with the specifics (the M2a-2b controllers
> depend on the trait shape).

**Files:**
- Modify: `crates/netlink/src/rtnetlink_backend.rs`
- Modify: `crates/netlink/src/lib.rs` (restore the module + re-export)
- Create: `crates/netlink/tests/netns.rs`

- [ ] **Step 1: Implement `RtNetlink`**

Replace `crates/netlink/src/rtnetlink_backend.rs` with:

```rust
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
    pub fn new() -> Result<Self> {
        let (connection, handle, _) =
            rtnetlink::new_connection().map_err(|e| NetlinkError::Netlink(e.to_string()))?;
        tokio::spawn(connection);
        Ok(Self { handle })
    }

    async fn link_index(&self, name: &str) -> Result<u32> {
        let mut links = self.handle.link().get().match_name(name.to_string()).execute();
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
            // netlink-packet-route 0.19 models flags as `Vec<LinkFlag>`, not a
            // bitflags `LinkFlags` — so test membership of `&LinkFlag::Up`.
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
        let mut addrs = self.handle.address().get().set_link_index_filter(index).execute();
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
        let mut addrs = self.handle.address().get().set_link_index_filter(index).execute();
        while let Some(msg) = addrs.try_next().await.map_err(net)? {
            let matches = msg.header.prefix_len == addr.prefix
                && msg.attributes.iter().any(|a| {
                    matches!(a, AddressAttribute::Address(ip) if *ip == addr.ip)
                });
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
        // `.priority()` is on the generic `RouteAddRequest<T>` (valid before
        // `.v4()`/`.v6()`), so applying it on the shared `add` binding wires the
        // metric into every arm below.
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
            (None, Some(IpAddr::V4(g))) => {
                add.v4().gateway(g).execute().await.map_err(net)
            }
            (None, Some(IpAddr::V6(g))) => {
                add.v6().gateway(g).execute().await.map_err(net)
            }
            (None, None) => Err(NetlinkError::Netlink("default route needs a gateway".into())),
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
```

> CONFIRMED during the spike: `rtnetlink` 0.14.1 does NOT re-export `netlink_packet_route` as a
> usable module path, so `netlink-packet-route = "0.19"` MUST be added as an explicit Linux-target
> dependency in both the root `[workspace.dependencies]` and `crates/netlink/Cargo.toml` (it resolves
> to the 0.19.0 already in the lock via rtnetlink — no new version pulled). The attribute enums used
> are `netlink_packet_route::link::{LinkAttribute, LinkFlag}` and
> `netlink_packet_route::address::AddressAttribute`.

- [ ] **Step 2: Restore the module + re-export in lib.rs**

In `crates/netlink/src/lib.rs`, restore:

```rust
pub mod fake;
#[cfg(target_os = "linux")]
pub mod rtnetlink_backend;
```

```rust
pub use fake::FakeNetworkBackend;
#[cfg(target_os = "linux")]
pub use rtnetlink_backend::RtNetlink;
```

- [ ] **Step 3: Build (this is the spike gate)**

Run: `cargo build -p machined-netlink`
Expected: PASS. If it does NOT compile against the installed `rtnetlink`, fix the API calls inside `RtNetlink` to match (per the SPIKE NOTE), re-run until it builds, and record every change in your report. Do not alter the `NetworkBackend` trait.

Run: `cargo clippy -p machined-netlink --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Write the netns integration test**

Create `crates/netlink/tests/netns.rs`:

```rust
//! Privileged integration test: drives the real `RtNetlink` backend inside a
//! fresh network namespace against a dummy link. Ignored by default; run with:
//!   sudo -E cargo test -p machined-netlink --test netns -- --ignored
//! or in CI under `unshare -rn`. Requires CAP_NET_ADMIN.

#![cfg(target_os = "linux")]

use std::net::{IpAddr, Ipv4Addr};

use machined_netlink::{NetworkBackend, RtNetlink};
use machined_resources::AddrCidr;

#[tokio::test]
#[ignore = "requires root + network namespace (CAP_NET_ADMIN)"]
async fn dummy_link_up_addr_roundtrip() {
    // Create an isolated namespace so we never touch the host network.
    // (Run the whole test under `unshare -rn`; here we assume we are already in
    // a fresh netns and create a dummy link via `ip`.)
    let status = std::process::Command::new("ip")
        .args(["link", "add", "mnd0", "type", "dummy"])
        .status()
        .expect("run ip link add");
    assert!(status.success(), "failed to create dummy link");

    let be = RtNetlink::new().expect("open netlink");

    // Bring it up, set MTU.
    be.set_link_up("mnd0", true).await.unwrap();
    be.set_mtu("mnd0", 1400).await.unwrap();
    let links = be.list_links().await.unwrap();
    let mnd0 = links.iter().find(|l| l.name == "mnd0").expect("link present");
    assert!(mnd0.up, "link should be up");
    assert_eq!(mnd0.mtu, 1400);

    // Add and read back an address.
    let addr = AddrCidr::new(IpAddr::V4(Ipv4Addr::new(10, 9, 9, 1)), 24);
    be.add_address("mnd0", addr).await.unwrap();
    let addrs = be.list_addresses("mnd0").await.unwrap();
    assert!(addrs.contains(&addr), "address should be present: {addrs:?}");

    // Delete it.
    be.del_address("mnd0", addr).await.unwrap();
    let addrs = be.list_addresses("mnd0").await.unwrap();
    assert!(!addrs.contains(&addr), "address should be removed");
}
```

- [ ] **Step 5: Confirm the default test run skips the netns test**

Run: `cargo test -p machined-netlink`
Expected: the fake tests pass and `dummy_link_up_addr_roundtrip` is listed as **ignored** (not run). The default `cargo test --workspace` stays root-free.

- [ ] **Step 6: (Best effort) run the privileged test**

If the environment allows, verify the real backend end-to-end:

Run: `sudo -E unshare -rn bash -c 'cargo test -p machined-netlink --test netns -- --ignored --nocapture'`
Expected: PASS — link up, MTU set, address round-trips. If the environment cannot run privileged tests, note that the netns test could not be executed locally and must run in CI; do NOT mark Task 3 fully verified without either a local pass or an explicit note.

- [ ] **Step 7: Full workspace gate + commit**

Run: `cargo build --workspace` → PASS.
Run: `cargo clippy --all-targets --all-features -- -D warnings` → clean.
Run: `cargo test --workspace` → all pass (netns test ignored).
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add crates/netlink Cargo.toml Cargo.lock
git commit -m "feat(netlink): RtNetlink real backend + netns integration test"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage (M2a spec, netlink portion):** `NetworkBackend` trait + `RtNetlink` + `FakeNetworkBackend` (Task 2, 3) ✓; `AddrCidr` parsing the config address strings (Task 1) ✓; netns-gated privileged integration test (Task 3) ✓; root-free unit tests via the fake (Task 2) ✓.
- **Deliberately deferred:** `del_route` is a no-op stub in M2a (routes are torn down with their link; full route deletion is M2b) — documented inline. The six controllers + machined wiring are **M2a-2b** (next plan), written against this `NetworkBackend` trait once it is real.
- **Spike risk is isolated:** only `RtNetlink` (Task 3) touches the external `rtnetlink` API. The trait, the fake, and all controller logic (M2a-2b) are deterministic and fully testable without it. If Task 3's API adaptation changes anything user-visible beyond `RtNetlink`'s internals, reflect it back into this plan and the M2a spec before M2a-2b is written.
- **Type consistency:** `AddrCidr` (from `resources`), `LinkState`/`RouteReq`/`NetlinkError` (new in `netlink`). The trait method set is the contract M2a-2b's controllers consume — keep it stable.
- **Placeholder scan:** none; Task 3's `rtnetlink` code is real best-effort code with an explicit spike protocol, not a placeholder.
