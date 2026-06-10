# machined-rs M2a-2b — Network Controllers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M2a-1 + M2a-2a complete on branch `spec/machined-rs-m2a-network`. Continue on it.

**Goal:** Add the `controllers` crate with the network controller pipeline (config → desired specs → applied to the kernel via `NetworkBackend`, with status published), and wire it into `machined` so a booted node configures its network from config. Completes M2a.

**Architecture:** Six controllers on the M0 runtime. `NetworkConfigController` translates the typed `network` config into owned desired specs via `reconcile_owned`. Five spec controllers each consume their spec as a strong input and use `reconcile_finalized` to apply (and revert on teardown) the real-world state through an injected `NetworkBackend`/`Platform`, publishing `*Status` resources. Unit-tested against `FakeNetworkBackend`; wired into `machined` with the real `RtNetlink`.

**Tech Stack:** Builds on M2a-1/M2a-2a. `runtime-core` (`Controller`, `reconcile_owned`, `reconcile_finalized`), `netlink` (`NetworkBackend`), `resources` (network specs/status), `config` (network section), `platform` (hostname).

---

## File Structure

```
crates/config/src/provider.rs       # MODIFY: add network() accessor
crates/controllers/
├── Cargo.toml                       # NEW
└── src/
    ├── lib.rs                       # NEW: re-export network module
    └── network/
        ├── mod.rs                   # NEW: shared helpers (publish_status, destroy_status, ctl, NS) + re-exports
        ├── config_controller.rs     # NEW: NetworkConfigController (config -> specs)
        ├── link.rs                  # NEW: LinkController
        ├── address.rs               # NEW: AddressController
        ├── route.rs                 # NEW: RouteController
        ├── hostname.rs              # NEW: HostnameController
        └── resolver.rs              # NEW: ResolverController
crates/machined/src/main.rs          # MODIFY: build backend, seed config resource, register controllers
crates/machined/tests/network.rs     # NEW: e2e config -> controllers -> fake backend -> status
```

The `controllers` crate uses one file per controller (each is a focused unit with one reconcile responsibility) plus a shared `mod.rs` for the small status helpers.

---

## Task 1: `Provider::network()` + controllers scaffold + NetworkConfigController

**Files:**
- Modify: `crates/config/src/provider.rs`
- Modify: `Cargo.toml` (workspace members + dep)
- Create: `crates/controllers/Cargo.toml`
- Create: `crates/controllers/src/lib.rs`
- Create: `crates/controllers/src/network/mod.rs`
- Create: `crates/controllers/src/network/config_controller.rs`

- [ ] **Step 1: Add `network()` accessor to Provider**

In `crates/config/src/provider.rs`, add `NetworkSection` to the import and a method. Replace the file's `use` line and add the method to the impl:

```rust
use crate::types::{MachineConfig, NetworkSection, ServiceConfig, Sysctl};
```

Add inside `impl Provider`:

```rust
    pub fn network(&self) -> &NetworkSection {
        &self.config.machine.network
    }
```

Verify: `cargo build -p machined-config && cargo test -p machined-config`.

- [ ] **Step 2: Add the controllers crate to the workspace**

In root `Cargo.toml`, add `"crates/controllers"` to `members` and add to `[workspace.dependencies]`:

```toml
machined-controllers = { path = "crates/controllers" }
```

Create `crates/controllers/Cargo.toml`:

```toml
[package]
name = "machined-controllers"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
machined-common.workspace = true
machined-config.workspace = true
machined-netlink.workspace = true
machined-platform.workspace = true
machined-resources.workspace = true
machined-runtime-core.workspace = true
async-trait.workspace = true
tracing.workspace = true

[dev-dependencies]
tokio = { workspace = true }
```

- [ ] **Step 3: Create the shared network helpers**

Create `crates/controllers/src/network/mod.rs`:

```rust
//! Network controllers: config -> desired specs -> applied kernel state.

pub mod address;
pub mod config_controller;
pub mod hostname;
pub mod link;
pub mod resolver;
pub mod route;

pub use address::AddressController;
pub use config_controller::NetworkConfigController;
pub use hostname::HostnameController;
pub use link::LinkController;
pub use resolver::ResolverController;
pub use route::RouteController;

use std::fmt::Display;

use machined_resources::{Key, Resource, ResourceObject, ResourceType};
use machined_runtime_core::{Error, State};

/// Namespace all network resources live in.
pub const NS: &str = "network";

/// Map any backend error into a runtime-core controller error.
pub(crate) fn ctl<E: Display>(e: E) -> Error {
    Error::Controller(e.to_string())
}

/// Create-or-update a status resource (`spec`) at `(NS, id)`.
pub(crate) fn publish_status(state: &State, id: &str, spec: Resource) {
    let key = Key::new(NS, spec.resource_type(), id);
    match state.get(&key) {
        Ok(existing) => {
            let _ = state.update(&key, existing.metadata.version, spec);
        }
        Err(_) => {
            let _ = state.create(ResourceObject::new(NS, id, spec));
        }
    }
}

/// Destroy a status resource at `(NS, typ, id)` if present.
pub(crate) fn destroy_status(state: &State, typ: ResourceType, id: &str) {
    let key = Key::new(NS, typ, id);
    if let Ok(obj) = state.get(&key) {
        let _ = state.destroy(&key, obj.metadata.version);
    }
}
```

- [ ] **Step 4: Write the failing NetworkConfigController test**

Create `crates/controllers/src/network/config_controller.rs`:

```rust
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
                    Err(_) => warn!(iface = %iface.name, addr = %addr_s, "invalid address, skipping"),
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
                let dest_label =
                    destination.map(|d| d.to_string()).unwrap_or_else(|| "default".to_string());
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
```

- [ ] **Step 5: Create the crate root (only the config controller wired for now)**

Create `crates/controllers/src/lib.rs`:

```rust
//! machined-rs controllers. Currently the network pipeline.

pub mod network;
```

To compile Task 1 alone, temporarily reduce `network/mod.rs` to ONLY the config controller: comment out the `pub mod address/hostname/link/resolver/route;` lines and their `pub use` re-exports (Tasks 2–3 restore them). Keep the helpers (`NS`, `ctl`, `publish_status`, `destroy_status`) and `pub mod config_controller; pub use config_controller::NetworkConfigController;`.

> Note: `ctl`/`publish_status`/`destroy_status` are unused until Tasks 2–3. To avoid `dead_code` warnings under `-D warnings` in this interim commit, add `#![allow(dead_code)]` at the top of `network/mod.rs` TEMPORARILY, and REMOVE it in Task 2 once the spec controllers use the helpers.

- [ ] **Step 6: Test + clippy + commit**

Run: `cargo test -p machined-controllers` → both config-controller tests pass.
Run: `cargo clippy -p machined-controllers --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add Cargo.toml Cargo.lock crates/config crates/controllers
git commit -m "feat(controllers): NetworkConfigController (config -> owned specs)"
```

---

## Task 2: LinkController + AddressController

**Files:**
- Create: `crates/controllers/src/network/link.rs`
- Create: `crates/controllers/src/network/address.rs`
- Modify: `crates/controllers/src/network/mod.rs` (restore link/address modules; remove the temp `allow(dead_code)`)

- [ ] **Step 1: Write the LinkController**

Create `crates/controllers/src/network/link.rs`:

```rust
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
                    backend.set_link_up(&spec.name, spec.up).await.map_err(ctl)?;
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
                    // Return the link to down; never delete it. A not-found link
                    // is already gone (benign); any other error is propagated so
                    // the finalizer is retained and revert retries next reconcile.
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
```

- [ ] **Step 2: Write the AddressController**

Create `crates/controllers/src/network/address.rs`:

```rust
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
                    // A not-found link is already gone (benign); other errors
                    // propagate so the finalizer is retained and revert retries.
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
        reconcile_owned(&state, "network-config", NS, ResourceType::AddressSpec, vec![]).unwrap();
        c.reconcile(&ctx).await.unwrap();
        assert!(backend.list_addresses("eth0").await.unwrap().is_empty());
    }
}
```

- [ ] **Step 3: Restore the modules in mod.rs**

In `crates/controllers/src/network/mod.rs`: remove the temporary `#![allow(dead_code)]`, and uncomment the `pub mod address;`/`pub mod link;` declarations and their `pub use` re-exports. (Leave `route`/`hostname`/`resolver` commented for Task 3.)

- [ ] **Step 4: Test + clippy + commit**

Run: `cargo test -p machined-controllers` → config (2) + link (3, incl. `revert_error_keeps_finalizer`) + address (1) tests pass.
Run: `cargo clippy -p machined-controllers --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add crates/controllers
git commit -m "feat(controllers): LinkController + AddressController (apply/revert/status)"
```

> **Review follow-up (applied):** the revert closures propagate non-`LinkNotFound` backend errors
> (rather than `let _ =` swallowing them) so a failed real-world revert retains the finalizer and
> retries — matching the apply path. `revert_error_keeps_finalizer` (with a `FailingRevertBackend`
> double) locks this in. The same pattern applies to RouteController in Task 3.

---

## Task 3: RouteController + HostnameController + ResolverController

**Files:**
- Create: `crates/controllers/src/network/route.rs`
- Create: `crates/controllers/src/network/hostname.rs`
- Create: `crates/controllers/src/network/resolver.rs`
- Modify: `crates/controllers/src/network/mod.rs` (restore the three modules)

- [ ] **Step 1: Write the RouteController**

Create `crates/controllers/src/network/route.rs`:

```rust
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
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::RouteSpec,
            kind: InputKind::Strong,
        }]
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
}
```

- [ ] **Step 2: Write the HostnameController**

Create `crates/controllers/src/network/hostname.rs`:

```rust
//! Applies the desired `HostnameSpec` via the platform.

use std::sync::Arc;

use async_trait::async_trait;
use machined_platform::Platform;
use machined_resources::{Resource, ResourceType};
use machined_runtime_core::{Controller, Input, InputKind, Output, ReconcileCtx};

use super::{ctl, NS};

pub struct HostnameController {
    platform: Arc<dyn Platform>,
}

impl HostnameController {
    pub fn new(platform: Arc<dyn Platform>) -> Self {
        Self { platform }
    }
}

#[async_trait]
impl Controller for HostnameController {
    fn name(&self) -> &str {
        "hostname"
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::HostnameSpec,
            // Weak: hostname is applied wholesale from config with no
            // per-instance teardown, so it does not finalize the spec.
            kind: InputKind::Weak,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        Vec::new()
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        // Hostname has no meaningful teardown; apply for any Running spec.
        for obj in ctx.state.list(NS, ResourceType::HostnameSpec) {
            if let Resource::HostnameSpec(s) = &obj.spec {
                self.platform.set_hostname(&s.hostname).map_err(ctl)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_platform::FakePlatform;
    use machined_resources::{HostnameSpec, ResourceObject};
    use machined_runtime_core::{ReconcileCtx, State};

    #[tokio::test]
    async fn sets_hostname() {
        let platform = Arc::new(FakePlatform::new());
        let state = State::new();
        state
            .create(ResourceObject::new(
                NS,
                "hostname",
                Resource::HostnameSpec(HostnameSpec {
                    hostname: "node-1".into(),
                }),
            ))
            .unwrap();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = HostnameController::new(platform.clone());
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(
            platform.recorded.lock().unwrap().hostname.as_deref(),
            Some("node-1")
        );
    }
}
```

- [ ] **Step 3: Write the ResolverController**

Create `crates/controllers/src/network/resolver.rs`:

```rust
//! Writes `/etc/resolv.conf` from the desired `ResolverSpec`.

use async_trait::async_trait;
use machined_resources::{Resource, ResourceType};
use machined_runtime_core::{Controller, Input, InputKind, Output, ReconcileCtx};

use super::{ctl, NS};

/// Controller writing resolv.conf. The path is injectable for tests.
pub struct ResolverController {
    path: std::path::PathBuf,
}

impl ResolverController {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Default production path.
    pub fn at_etc() -> Self {
        Self::new("/etc/resolv.conf")
    }
}

#[async_trait]
impl Controller for ResolverController {
    fn name(&self) -> &str {
        "resolver"
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::ResolverSpec,
            // Weak: resolv.conf is rewritten wholesale from config with no
            // per-instance teardown, so it does not finalize the spec.
            kind: InputKind::Weak,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        Vec::new()
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        for obj in ctx.state.list(NS, ResourceType::ResolverSpec) {
            if let Resource::ResolverSpec(s) = &obj.spec {
                let mut body = String::new();
                for sd in &s.search {
                    body.push_str(&format!("search {sd}\n"));
                }
                for ns in &s.nameservers {
                    body.push_str(&format!("nameserver {ns}\n"));
                }
                // Atomic write: temp file + rename.
                let tmp = self.path.with_extension("tmp");
                std::fs::write(&tmp, &body).map_err(ctl)?;
                std::fs::rename(&tmp, &self.path).map_err(ctl)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{ResolverSpec, ResourceObject};
    use machined_runtime_core::{ReconcileCtx, State};
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn writes_resolv_conf() {
        let dir = std::env::temp_dir().join(format!("mnd-resolv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resolv.conf");

        let state = State::new();
        state
            .create(ResourceObject::new(
                NS,
                "resolver",
                Resource::ResolverSpec(ResolverSpec {
                    nameservers: vec![IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))],
                    search: vec!["example.com".into()],
                }),
            ))
            .unwrap();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = ResolverController::new(&path);
        c.reconcile(&ctx).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("search example.com"));
        assert!(written.contains("nameserver 1.1.1.1"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 4: Restore the remaining modules**

In `crates/controllers/src/network/mod.rs`, uncomment the `pub mod route;`/`pub mod hostname;`/`pub mod resolver;` declarations and their `pub use` re-exports. The file's module section should now match the full version shown in Task 1 Step 3 (all six modules active).

- [ ] **Step 5: Test + clippy + commit**

Run: `cargo test -p machined-controllers` → all controller tests pass.
Run: `cargo clippy -p machined-controllers --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add crates/controllers
git commit -m "feat(controllers): Route + Hostname + Resolver controllers"
```

---

## Task 4: Wire controllers into machined + end-to-end test

**Files:**
- Modify: `crates/machined/Cargo.toml` (add netlink + controllers deps)
- Modify: `crates/machined/src/main.rs`
- Create: `crates/machined/tests/network.rs`

- [ ] **Step 1: Add deps to machined**

In `crates/machined/Cargo.toml` `[dependencies]`, add:

```toml
machined-controllers.workspace = true
machined-netlink.workspace = true
```

- [ ] **Step 2: Register the controllers in run_daemon**

In `crates/machined/src/main.rs`, add imports:

```rust
use machined_controllers::network::{
    AddressController, HostnameController, LinkController, NetworkConfigController,
    ResolverController, RouteController,
};
use std::sync::Arc;
```

(`std::sync::Arc` may already be imported — keep a single import.)

Add a helper to build the network backend, mirroring `build_platform`:

```rust
fn build_network_backend() -> Arc<dyn machined_netlink::NetworkBackend> {
    #[cfg(target_os = "linux")]
    {
        match machined_netlink::RtNetlink::new() {
            Ok(b) => Arc::new(b),
            Err(e) => {
                error!("failed to open netlink ({e}); using inert fake backend");
                Arc::new(machined_netlink::FakeNetworkBackend::new())
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_netlink::FakeNetworkBackend::new())
    }
}
```

In `run_daemon`, AFTER `let provider = Provider::new(config);` and BEFORE the runtime is spawned, register the controllers. Currently the code builds `let runtime = Runtime::new();` then spawns it. Change that section so controllers are registered before `runtime.run`:

Replace:
```rust
    // Build the shared runtime + service manager.
    let runtime = Runtime::new();
    let state = runtime.state();
    let services = Arc::new(Mutex::new(ServiceManager::new(state.clone())));

    // Spawn the reconcile runtime (no controllers in M1; the loop is live and
    // ready for M2 controllers).
    let rt_token = shutdown.clone();
    let rt_handle = tokio::spawn(async move {
        if let Err(e) = runtime.run(rt_token).await {
            error!("runtime error: {e}");
        }
    });

    // Load config (fall back to an empty config if the file is absent, so a
    // bare boot still comes up).
    let config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let (config, _raw) = match load_from_path(&config_path) {
        Ok(v) => v,
        Err(e) => {
            info!("no config at {} ({e}); booting with defaults", config_path.display());
            (Default::default(), String::new())
        }
    };
    let provider = Provider::new(config);
```

with:
```rust
    // Load config first so the controllers can be built from it (fall back to
    // an empty config if the file is absent, so a bare boot still comes up).
    let config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let (config, _raw) = match load_from_path(&config_path) {
        Ok(v) => v,
        Err(e) => {
            info!("no config at {} ({e}); booting with defaults", config_path.display());
            (Default::default(), String::new())
        }
    };
    let provider = Provider::new(config);

    // Build the shared runtime + service manager, registering the network
    // controllers so the node configures its network from config.
    let mut runtime = Runtime::new();
    let state = runtime.state();
    let services = Arc::new(Mutex::new(ServiceManager::new(state.clone())));

    let net_backend = build_network_backend();
    runtime.register(Box::new(NetworkConfigController::new(provider.clone())));
    runtime.register(Box::new(LinkController::new(net_backend.clone())));
    runtime.register(Box::new(AddressController::new(net_backend.clone())));
    runtime.register(Box::new(RouteController::new(net_backend.clone())));
    runtime.register(Box::new(HostnameController::new(platform.clone())));
    runtime.register(Box::new(ResolverController::at_etc()));

    let rt_token = shutdown.clone();
    let rt_handle = tokio::spawn(async move {
        if let Err(e) = runtime.run(rt_token).await {
            error!("runtime error: {e}");
        }
    });
```

> `Provider` must be `Clone` (it derives `Clone`) so `provider.clone()` works for the controller and the later `SequencerCtx`. The subsequent `SequencerCtx { ... provider, ... }` keeps using the original `provider`.

- [ ] **Step 3: Build + smoke-test**

Run: `cargo build --workspace`
Expected: PASS.

Run: `cargo run -p machined -- version`
Expected: `machined 0.1.0`.

- [ ] **Step 4: Write the end-to-end network test**

This test drives the controllers through the real `Runtime` against a `FakeNetworkBackend`, proving config → specs → applied kernel state → status, without root.

Create `crates/machined/tests/network.rs`:

```rust
//! End-to-end: a network config, run through the controllers on the real
//! Runtime against a fake backend, configures the (simulated) kernel and
//! publishes status — no root required.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{
    InterfaceConfig, MachineConfig, MachineSection, NetworkSection, Provider, RouteConfig,
};
use machined_controllers::network::{
    AddressController, LinkController, NetworkConfigController, RouteController, NS,
};
use machined_netlink::{FakeNetworkBackend, NetworkBackend};
use machined_resources::{Key, ResourceType};
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn config_drives_network_through_controllers() {
    let backend = Arc::new(FakeNetworkBackend::new().with_link("eth0", 1500));

    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: NetworkSection {
                interfaces: vec![InterfaceConfig {
                    name: "eth0".into(),
                    up: true,
                    mtu: Some(9000),
                    addresses: vec!["10.0.0.5/24".into()],
                    routes: vec![RouteConfig {
                        to: None,
                        via: "10.0.0.1".parse().unwrap(),
                        metric: None,
                    }],
                }],
                nameservers: vec![],
                search: vec![],
            },
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(NetworkConfigController::new(Provider::new(config))));
    runtime.register(Box::new(LinkController::new(backend.clone())));
    runtime.register(Box::new(AddressController::new(backend.clone())));
    runtime.register(Box::new(RouteController::new(backend.clone())));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    // Poll until the link is up + mtu applied + address present (the config
    // controller's initial reconcile creates specs, which wake the spec
    // controllers).
    let mut ok = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let links = backend.list_links().await.unwrap();
        let addrs = backend.list_addresses("eth0").await.unwrap();
        if links.first().map(|l| l.up && l.mtu == 9000) == Some(true)
            && addrs.iter().any(|a| a.to_string() == "10.0.0.5/24")
            && backend.routes().len() == 1
        {
            ok = true;
            break;
        }
    }
    assert!(ok, "network was not fully configured through the controllers");

    // All three status resources published (the live-runtime status path).
    assert!(state
        .get(&Key::new(NS, ResourceType::LinkStatus, "eth0"))
        .is_ok());
    assert!(state
        .get(&Key::new(NS, ResourceType::AddressStatus, "eth0/10.0.0.5/24"))
        .is_ok());
    assert!(state
        .get(&Key::new(NS, ResourceType::RouteStatus, "eth0/default/10.0.0.1"))
        .is_ok());

    shutdown.cancel();
    let _ = handle.await;
}
```

> `machined_controllers::network::NS` is `pub`, so the test imports it directly (no namespace
> string duplicated). The address/route status ids are the deterministic ids the config controller
> assigns: `eth0/<cidr>` and `eth0/default/<gateway>`.

- [ ] **Step 5: Run the e2e test + full gate**

Run: `cargo test -p machined --test network`
Expected: PASS — link up, mtu 9000, address 10.0.0.5/24, one route, link status published.

Run: `make pre-commit`
Expected: PASS — fmt, clippy -D warnings, and the full workspace test suite all green (netns test still ignored).

- [ ] **Step 6: Commit**

```bash
git add crates/machined
git commit -m "feat(machined): register network controllers + e2e network test"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage (M2a spec, controllers portion):** `NetworkConfigController` config→specs with owner-GC (Task 1) ✓; `Link/Address/Route` controllers apply+revert+status via `reconcile_finalized` against `NetworkBackend` (Tasks 2–3) ✓; `Hostname` (platform) + `Resolver` (resolv.conf) controllers (Task 3) ✓; machined wiring with real `RtNetlink` + fallback (Task 4) ✓; root-free e2e test through the real `Runtime` against the fake (Task 4) ✓.
- **Completes M2a.** After this, the M2a network milestone (M2a-1 + M2a-2a + M2a-2b) is done; the branch can be merged.
- **Deliberate M2a limits (per spec):** `del_route` is a backend no-op (routes effectively add-only this milestone); hostname/resolver have no revert (no finalizer) — acceptable since they are singletons fully determined by config; live rtnetlink-monitor status is deferred (status is what each controller applied/read-back, not an independent watch).
- **Type consistency:** uses `Controller`/`Input`/`InputKind::Strong`/`Output`/`OutputKind::Exclusive`/`ReconcileCtx`/`reconcile_owned`/`reconcile_finalized` from runtime-core; `NetworkBackend`/`RouteReq`/`FakeNetworkBackend`/`RtNetlink`/`routes()` from netlink; the network specs/status + `AddrCidr` parse from resources; `Provider::network()` from config; `Platform::set_hostname` from platform. The `reconcile_finalized` closures clone the spec out of `&ResourceObject` before `.await` per its documented contract.
- **Placeholder scan:** none; every step ships complete code + exact commands.
