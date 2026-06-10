# machined-rs M2a-1 — owner-cascade Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M0 + M1 complete on `main`. Work on branch `spec/machined-rs-m2a-network`.

**Goal:** Build the reusable owner-cascade machinery in `runtime-core` (owner stamping, `reconcile_owned` GC, `reconcile_finalized` apply/revert protocol) plus the network `Resource` types and the `config` network section — the framework + data the M2a-2 controllers will consume. No netlink yet.

**Architecture:** Controllers that own resources need: (1) a way to stamp ownership on create, (2) a GC that tears down owned resources no longer desired, and (3) a finalizer-gated apply/revert loop so a desired resource isn't destroyed until the real-world state it produced is reverted. This plan adds those three to `runtime-core` generically (tested against existing resource types), adds the typed network resources, and extends the machine config with a `network` section.

**Tech Stack:** Builds on M0/M1. Pure Rust — `std::net`, `serde`, `tokio` (for the async `reconcile_finalized`). No external system APIs, so no privileged tests in this plan.

---

## File Structure

```
crates/resources/src/
├── network.rs          # NEW: AddrCidr + network specs/status structs
├── resource.rs         # MODIFY: add 8 Resource variants + resource_type() arms
├── metadata.rs         # MODIFY: add 8 ResourceType variants + Display arms
└── lib.rs              # MODIFY: re-export network types
crates/config/src/
├── types.rs            # MODIFY: add NetworkSection/InterfaceConfig/RouteConfig + field on MachineSection
└── lib.rs              # MODIFY: re-export new config types
crates/runtime-core/src/
├── owned.rs            # NEW: reconcile_owned + reconcile_finalized free functions
├── runtime.rs          # MODIFY: add ReconcileCtx::create_owned
└── lib.rs              # MODIFY: re-export owned helpers
```

---

## Task 1: Network resource types in `resources`

**Files:**
- Create: `crates/resources/src/network.rs`
- Modify: `crates/resources/src/metadata.rs`
- Modify: `crates/resources/src/resource.rs`
- Modify: `crates/resources/src/lib.rs`

- [ ] **Step 1: Add the new ResourceType variants**

In `crates/resources/src/metadata.rs`, replace the `ResourceType` enum and its `Display` impl with:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceType {
    MachineConfig,
    ServiceStatus,
    LinkSpec,
    AddressSpec,
    RouteSpec,
    HostnameSpec,
    ResolverSpec,
    LinkStatus,
    AddressStatus,
    RouteStatus,
}

impl fmt::Display for ResourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ResourceType::MachineConfig => "MachineConfig",
            ResourceType::ServiceStatus => "ServiceStatus",
            ResourceType::LinkSpec => "LinkSpec",
            ResourceType::AddressSpec => "AddressSpec",
            ResourceType::RouteSpec => "RouteSpec",
            ResourceType::HostnameSpec => "HostnameSpec",
            ResourceType::ResolverSpec => "ResolverSpec",
            ResourceType::LinkStatus => "LinkStatus",
            ResourceType::AddressStatus => "AddressStatus",
            ResourceType::RouteStatus => "RouteStatus",
        };
        f.write_str(s)
    }
}
```

- [ ] **Step 2: Write the failing test for the network types**

Create `crates/resources/src/network.rs`:

```rust
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
```

- [ ] **Step 3: Run the network test to verify it fails**

Run: `cargo test -p machined-resources network::tests::addr_cidr_display`
Expected: FAIL — `network` module not declared yet (compile error).

- [ ] **Step 4: Add the Resource enum variants**

In `crates/resources/src/resource.rs`, add this import near the top (below the existing `use` line):

```rust
use crate::network::{
    AddressSpec, AddressStatus, HostnameSpec, LinkSpec, LinkStatus, ResolverSpec, RouteSpec,
    RouteStatus,
};
```

Replace the `Resource` enum and its `resource_type` impl with:

```rust
/// The closed set of resource specs. Each variant's payload is its typed spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resource {
    MachineConfig(MachineConfigSpec),
    ServiceStatus(ServiceStatusSpec),
    LinkSpec(LinkSpec),
    AddressSpec(AddressSpec),
    RouteSpec(RouteSpec),
    HostnameSpec(HostnameSpec),
    ResolverSpec(ResolverSpec),
    LinkStatus(LinkStatus),
    AddressStatus(AddressStatus),
    RouteStatus(RouteStatus),
}

impl Resource {
    /// The `ResourceType` discriminant for this spec.
    pub fn resource_type(&self) -> ResourceType {
        match self {
            Resource::MachineConfig(_) => ResourceType::MachineConfig,
            Resource::ServiceStatus(_) => ResourceType::ServiceStatus,
            Resource::LinkSpec(_) => ResourceType::LinkSpec,
            Resource::AddressSpec(_) => ResourceType::AddressSpec,
            Resource::RouteSpec(_) => ResourceType::RouteSpec,
            Resource::HostnameSpec(_) => ResourceType::HostnameSpec,
            Resource::ResolverSpec(_) => ResourceType::ResolverSpec,
            Resource::LinkStatus(_) => ResourceType::LinkStatus,
            Resource::AddressStatus(_) => ResourceType::AddressStatus,
            Resource::RouteStatus(_) => ResourceType::RouteStatus,
        }
    }
}
```

- [ ] **Step 5: Wire the module + re-exports**

In `crates/resources/src/lib.rs`, add the module declaration and re-exports. The file should read:

```rust
//! Pure resource data model for machined-rs: metadata and the closed
//! `Resource` enum. No I/O, no async.

pub mod metadata;
pub mod network;
pub mod resource;

pub use metadata::{Key, Metadata, Phase, ResourceType};
pub use network::{
    AddrCidr, AddressSpec, AddressStatus, HostnameSpec, LinkSpec, LinkStatus, ResolverSpec,
    RouteSpec, RouteStatus,
};
pub use resource::{
    MachineConfigSpec, Resource, ResourceObject, ServiceState, ServiceStatusSpec,
};
```

- [ ] **Step 6: Run tests + clippy to verify**

Run: `cargo test -p machined-resources`
Expected: PASS — existing tests + `addr_cidr_display` pass.

Run: `cargo clippy -p machined-resources --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add crates/resources
git commit -m "feat(resources): network spec + status resource types"
```

---

## Task 2: `network` section in `config`

**Files:**
- Modify: `crates/config/src/types.rs`
- Modify: `crates/config/src/lib.rs`

- [ ] **Step 1: Write the failing test for network config parsing**

Append to the `tests` module in `crates/config/src/load.rs` (inside the existing `#[cfg(test)] mod tests { ... }`), a new test and sample:

```rust
    const NET_SAMPLE: &str = r#"
machine:
  network:
    interfaces:
      - name: eth0
        mtu: 1500
        addresses:
          - 192.168.1.10/24
        routes:
          - to: 0.0.0.0/0
            via: 192.168.1.1
    nameservers:
      - 1.1.1.1
      - 8.8.8.8
    search:
      - example.com
"#;

    #[test]
    fn parses_network_section() {
        let cfg = load_from_str(NET_SAMPLE).unwrap();
        let net = &cfg.machine.network;
        assert_eq!(net.interfaces.len(), 1);
        let eth0 = &net.interfaces[0];
        assert_eq!(eth0.name, "eth0");
        assert!(eth0.up, "up defaults to true");
        assert_eq!(eth0.mtu, Some(1500));
        assert_eq!(eth0.addresses, vec!["192.168.1.10/24".to_string()]);
        assert_eq!(eth0.routes.len(), 1);
        assert_eq!(eth0.routes[0].to.as_deref(), Some("0.0.0.0/0"));
        assert_eq!(net.nameservers.len(), 2);
        assert_eq!(net.search, vec!["example.com".to_string()]);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-config parses_network_section`
Expected: FAIL — `network` field / types do not exist (compile error).

- [ ] **Step 3: Add the network config types**

In `crates/config/src/types.rs`, add this import at the top (below the existing `use serde::Deserialize;`):

```rust
use std::net::IpAddr;
```

Add a `network` field to `MachineSection` (insert before the closing brace of the struct, after `services`):

```rust
    /// Node network configuration.
    #[serde(default)]
    pub network: NetworkSection,
```

Append these new types to the end of `types.rs`:

```rust
/// Static node network configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    #[serde(default)]
    pub interfaces: Vec<InterfaceConfig>,
    #[serde(default)]
    pub nameservers: Vec<IpAddr>,
    #[serde(default)]
    pub search: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceConfig {
    pub name: String,
    /// Admin state; defaults to up.
    #[serde(default = "default_true")]
    pub up: bool,
    #[serde(default)]
    pub mtu: Option<u32>,
    /// Addresses in `ip/prefix` form (parsed by the network controller).
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Destination CIDR; `None`/absent or `0.0.0.0/0` means default route.
    #[serde(default)]
    pub to: Option<String>,
    /// Gateway IP.
    pub via: IpAddr,
    #[serde(default)]
    pub metric: Option<u32>,
}
```

- [ ] **Step 4: Re-export the new types**

In `crates/config/src/lib.rs`, update the `types` re-export line to include the new types:

```rust
pub use types::{
    InterfaceConfig, MachineConfig, MachineSection, NetworkSection, RestartPolicy, RouteConfig,
    ServiceConfig, Sysctl,
};
```

- [ ] **Step 5: Update existing `MachineSection { ... }` literals (the new field breaks them)**

Adding the `network` field makes every explicit `MachineSection { ... }` struct literal non-exhaustive (E0063). Two test sites from M1 construct it fully and must add the field. In `crates/sequencer/src/boot.rs` (the `boot_mounts_and_starts_services` test) and `crates/machined/tests/boot_harness.rs` (the `boots_supervises_and_shuts_down` test), add `network: Default::default(),` to the `MachineSection { ... }` literal (after the `services: vec![...]` field).

Run: `cargo build --workspace` — confirm both crates compile.

- [ ] **Step 5: Run tests + clippy**

Run: `cargo test -p machined-config`
Expected: PASS — existing 3 tests + `parses_network_section`.

Run: `cargo clippy -p machined-config --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/config
git commit -m "feat(config): static network section (interfaces, routes, dns)"
```

---

## Task 3: `reconcile_owned` + `create_owned` in runtime-core

**Files:**
- Create: `crates/runtime-core/src/owned.rs`
- Modify: `crates/runtime-core/src/runtime.rs`
- Modify: `crates/runtime-core/src/lib.rs`

- [ ] **Step 1: Add `create_owned` to ReconcileCtx**

In `crates/runtime-core/src/runtime.rs`, find the `ReconcileCtx` struct and add an impl block directly after it:

```rust
impl ReconcileCtx {
    /// Create a resource stamped as owned by `owner`. The owner is recorded in
    /// metadata so `reconcile_owned` can later garbage-collect it.
    pub fn create_owned(
        &self,
        owner: &str,
        mut object: machined_resources::ResourceObject,
    ) -> crate::error::Result<()> {
        object.metadata.owner = Some(owner.to_string());
        self.state.create(object)
    }
}
```

- [ ] **Step 2: Write the failing tests for reconcile_owned**

Create `crates/runtime-core/src/owned.rs`:

```rust
//! Owner-cascade helpers: a controller owns the resources it creates,
//! garbage-collects ones no longer desired, and (via `reconcile_finalized`)
//! holds a desired resource alive until its consumer reverts the real-world
//! state it produced.

use std::collections::HashSet;
use std::future::Future;

use machined_resources::{Phase, ResourceObject, ResourceType};

use crate::error::Result;
use crate::state::State;

/// Reconcile the full set of resources of one `(namespace, typ)` that `owner`
/// should have. Upserts each `desired` resource (stamping ownership on create),
/// and for each existing resource owned by `owner` whose id is not in `desired`,
/// tears it down — destroying it once no finalizers remain.
///
/// `desired` must all share `namespace` and `typ`; ids must be unique.
pub fn reconcile_owned(
    state: &State,
    owner: &str,
    namespace: &str,
    typ: ResourceType,
    desired: Vec<ResourceObject>,
) -> Result<()> {
    debug_assert!(
        desired
            .iter()
            .all(|o| o.metadata.namespace == namespace && o.metadata.typ == typ),
        "reconcile_owned: every desired object must match the namespace/typ args"
    );
    let desired_ids: HashSet<String> =
        desired.iter().map(|o| o.metadata.id.clone()).collect();

    // Upsert desired.
    for obj in desired {
        let key = obj.metadata.key();
        match state.get(&key) {
            Ok(existing) => {
                if existing.spec != obj.spec {
                    state.update(&key, existing.metadata.version, obj.spec)?;
                }
            }
            Err(crate::error::Error::NotFound(_)) => {
                let mut owned = obj;
                owned.metadata.owner = Some(owner.to_string());
                state.create(owned)?;
            }
            Err(e) => return Err(e),
        }
    }

    // GC owned resources no longer desired.
    for existing in state.list(namespace, typ) {
        let owned_by_us = existing.metadata.owner.as_deref() == Some(owner);
        if owned_by_us && !desired_ids.contains(&existing.metadata.id) {
            let key = existing.metadata.key();
            let ready = state.teardown(&key)?;
            if ready {
                // Re-read for the current version bumped by teardown.
                let cur = state.get(&key)?;
                state.destroy(&key, cur.metadata.version)?;
            }
        }
    }

    Ok(())
}

/// Apply/revert a controller's strong inputs under a finalizer. For each input
/// in `Running`, ensures the finalizer is present then calls `apply`; for each
/// in `TearingDown`, calls `revert` then removes the finalizer (releasing the
/// resource for destruction).
///
/// `apply` and `revert` must be idempotent: after a crash either may be
/// re-invoked on the next reconcile (the finalizer is added before `apply` and
/// removed only after `revert` succeeds). Clone what you need out of the
/// `&ResourceObject` before `.await` — the returned future must not borrow it.
pub async fn reconcile_finalized<A, R, AFut, RFut>(
    state: &State,
    finalizer: &str,
    inputs: &[ResourceObject],
    mut apply: A,
    mut revert: R,
) -> Result<()>
where
    A: FnMut(&ResourceObject) -> AFut,
    R: FnMut(&ResourceObject) -> RFut,
    AFut: Future<Output = Result<()>>,
    RFut: Future<Output = Result<()>>,
{
    for obj in inputs {
        let key = obj.metadata.key();
        match obj.metadata.phase {
            Phase::Running => {
                state.add_finalizer(&key, finalizer)?;
                apply(obj).await?;
            }
            Phase::TearingDown => {
                revert(obj).await?;
                state.remove_finalizer(&key, finalizer)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{
        Key, Resource, ResourceObject, ServiceState, ServiceStatusSpec,
    };
    use std::sync::{Arc, Mutex};

    const NS: &str = "runtime";

    fn svc(id: &str) -> ResourceObject {
        ResourceObject::new(
            NS,
            id,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: id.into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        )
    }

    fn key(id: &str) -> Key {
        Key::new(NS, ResourceType::ServiceStatus, id)
    }

    #[test]
    fn reconcile_owned_creates_and_stamps_owner() {
        let state = State::new();
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![svc("a")])
            .unwrap();
        let got = state.get(&key("a")).unwrap();
        assert_eq!(got.metadata.owner.as_deref(), Some("ctl"));
    }

    #[test]
    fn reconcile_owned_gcs_removed_without_finalizers() {
        let state = State::new();
        reconcile_owned(
            &state,
            "ctl",
            NS,
            ResourceType::ServiceStatus,
            vec![svc("a"), svc("b")],
        )
        .unwrap();
        // Second pass drops "b" from desired → it should be destroyed.
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![svc("a")])
            .unwrap();
        assert!(state.get(&key("a")).is_ok());
        assert!(matches!(
            state.get(&key("b")),
            Err(crate::error::Error::NotFound(_))
        ));
    }

    #[test]
    fn reconcile_owned_holds_removed_with_finalizer() {
        let state = State::new();
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![svc("a")])
            .unwrap();
        state.add_finalizer(&key("a"), "consumer").unwrap();
        // Drop "a" from desired → finalizer holds it in TearingDown, not destroyed.
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![]).unwrap();
        let held = state.get(&key("a")).unwrap();
        assert_eq!(held.metadata.phase, Phase::TearingDown);
        // After the consumer clears its finalizer, a further pass destroys it.
        state.remove_finalizer(&key("a"), "consumer").unwrap();
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![]).unwrap();
        assert!(matches!(
            state.get(&key("a")),
            Err(crate::error::Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn reconcile_finalized_applies_running_and_reverts_tearing_down() {
        let state = State::new();
        state.create(svc("running")).unwrap();
        // Build a TearingDown input: create, finalize, teardown.
        state.create(svc("dying")).unwrap();
        state.add_finalizer(&key("dying"), "net").unwrap();
        state.teardown(&key("dying")).unwrap();

        let inputs = vec![
            state.get(&key("running")).unwrap(),
            state.get(&key("dying")).unwrap(),
        ];

        let applied = Arc::new(Mutex::new(Vec::<String>::new()));
        let reverted = Arc::new(Mutex::new(Vec::<String>::new()));
        let a = applied.clone();
        let r = reverted.clone();

        reconcile_finalized(
            &state,
            "net",
            &inputs,
            move |obj| {
                let a = a.clone();
                let id = obj.metadata.id.clone();
                async move {
                    a.lock().unwrap().push(id);
                    Ok(())
                }
            },
            move |obj| {
                let r = r.clone();
                let id = obj.metadata.id.clone();
                async move {
                    r.lock().unwrap().push(id);
                    Ok(())
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(*applied.lock().unwrap(), vec!["running".to_string()]);
        assert_eq!(*reverted.lock().unwrap(), vec!["dying".to_string()]);
        // Running input got the finalizer added.
        assert!(state
            .get(&key("running"))
            .unwrap()
            .metadata
            .finalizers
            .contains(&"net".to_string()));
        // Dying input had its finalizer removed.
        assert!(!state
            .get(&key("dying"))
            .unwrap()
            .metadata
            .finalizers
            .contains(&"net".to_string()));
    }

    fn svc_with_msg(id: &str, msg: &str) -> ResourceObject {
        ResourceObject::new(
            NS,
            id,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: id.into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: msg.into(),
            }),
        )
    }

    #[test]
    fn reconcile_owned_no_op_on_equal_spec() {
        let state = State::new();
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![svc("a")]).unwrap();
        let v1 = state.get(&key("a")).unwrap().metadata.version;
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![svc("a")]).unwrap();
        let v2 = state.get(&key("a")).unwrap().metadata.version;
        assert_eq!(v1, v2, "unchanged desired must not bump version");
    }

    #[test]
    fn reconcile_owned_updates_changed_spec() {
        let state = State::new();
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![svc("a")]).unwrap();
        let v1 = state.get(&key("a")).unwrap().metadata.version;
        reconcile_owned(
            &state,
            "ctl",
            NS,
            ResourceType::ServiceStatus,
            vec![svc_with_msg("a", "changed")],
        )
        .unwrap();
        let got = state.get(&key("a")).unwrap();
        assert!(got.metadata.version > v1, "changed spec must bump version");
        match got.spec {
            Resource::ServiceStatus(s) => assert_eq!(s.last_message, "changed"),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn reconcile_owned_leaves_other_owners_and_unowned() {
        let state = State::new();
        reconcile_owned(&state, "other", NS, ResourceType::ServiceStatus, vec![svc("x")]).unwrap();
        state.create(svc("y")).unwrap(); // unowned
        reconcile_owned(&state, "ctl", NS, ResourceType::ServiceStatus, vec![]).unwrap();
        assert!(state.get(&key("x")).is_ok(), "other-owned survives ctl GC");
        assert!(state.get(&key("y")).is_ok(), "unowned survives ctl GC");
    }

    #[tokio::test]
    async fn reconcile_finalized_apply_error_keeps_finalizer() {
        let state = State::new();
        state.create(svc("a")).unwrap();
        let inputs = vec![state.get(&key("a")).unwrap()];
        let res = reconcile_finalized(
            &state,
            "net",
            &inputs,
            |_obj| async { Err(crate::error::Error::Controller("boom".into())) },
            |_obj| async { Ok(()) },
        )
        .await;
        assert!(res.is_err(), "apply error propagates");
        assert!(
            state
                .get(&key("a"))
                .unwrap()
                .metadata
                .finalizers
                .contains(&"net".to_string()),
            "finalizer added before apply must remain for retry"
        );
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p machined-runtime-core owned::`
Expected: FAIL — `owned` module not declared yet (compile error).

- [ ] **Step 4: Wire the module + re-exports**

In `crates/runtime-core/src/lib.rs`, add the module + re-export. The file should read:

```rust
//! Generic, statically-typed reconcile runtime for machined-rs.
//!
//! Provides an in-memory resource [`State`] store with COSI semantics
//! (versioned CAS updates, finalizers, owner refs, teardown), a broadcast
//! watch bus, and a [`Runtime`] that drives one reconcile loop per
//! registered [`Controller`].

pub mod error;
pub mod owned;
pub mod runtime;
pub mod state;
pub mod watch;

pub use error::{Error, Result};
pub use owned::{reconcile_finalized, reconcile_owned};
pub use runtime::{Controller, Input, InputKind, Output, OutputKind, ReconcileCtx, Runtime};
pub use state::State;
pub use watch::{Event, EventKind};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p machined-runtime-core owned::`
Expected: PASS — all four `owned` tests pass.

- [ ] **Step 6: Full build + lint + test + commit**

Run: `cargo build --workspace`
Expected: PASS.

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: clean.

Run: `cargo test --workspace`
Expected: PASS — all existing + new tests.

```bash
cargo fmt --all
git add crates/runtime-core
git commit -m "feat(runtime-core): owner-cascade (create_owned, reconcile_owned, reconcile_finalized)"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage (M2a-1 portion of the M2a spec):**
  - Network resource types (LinkSpec/AddressSpec/RouteSpec/HostnameSpec/ResolverSpec + LinkStatus/AddressStatus/RouteStatus + AddrCidr) — Task 1 ✓
  - config `network` section (interfaces, addresses, routes, nameservers, search) — Task 2 ✓
  - owner-cascade: owner stamping (`create_owned`), `reconcile_owned` GC, `reconcile_finalized` apply/revert protocol, all unit-tested — Task 3 ✓
- **Deliberately deferred to M2a-2:** the `netlink` crate, the six controllers, machined wiring, the netns integration test. This plan is framework + types only — no privileged tests, no external system APIs.
- **Note on `reconcile_finalized` signature:** it is an async free function over `&State` taking `apply`/`revert` async closures (`FnMut -> impl Future<Output = Result<()>>`), because the M2a-2 spec controllers' apply is async (netlink). If the implementing/reviewing pass finds the generic-async-closure ergonomics awkward in practice, that refinement is in-scope here and MUST be reflected back into the M2a spec before M2a-2's plan is written (the M2a-2 controllers depend on this exact signature).
- **Type consistency:** uses M0 APIs exactly — `State::{get,list,create,update,teardown,destroy,add_finalizer,remove_finalizer}`, `Error::NotFound`, `Phase::{Running,TearingDown}`, `ResourceObject::new`, `Key::new`, `metadata.{owner,version,phase,finalizers,key()}`.
- **Placeholder scan:** none; every step has complete code + exact commands.
