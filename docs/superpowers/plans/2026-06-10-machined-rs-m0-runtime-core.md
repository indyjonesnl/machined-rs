# machined-rs M0 — runtime-core Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the generic, statically-typed reconcile runtime (`runtime-core`) plus its supporting crates (`common`, `resources`) — a COSI-semantics resource store, watch bus, and controller loop — as the foundation every machined-rs subsystem sits on.

**Architecture:** A closed-enum resource model (`Resource`) with COSI metadata (namespace/type/id/version/owner/finalizers/phase). An in-memory `State` store provides get/list/create/CAS-update/destroy + finalizers + teardown, and broadcasts change events. A `Runtime` registers `Controller`s and drives one reconcile loop per controller, woken by its declared input watches. No protobuf-`Any`, no reflection — adding a resource type edits the enum and the compiler enforces wiring.

**Tech Stack:** Rust (edition 2021), `tokio` 1.40, `tokio-util` (CancellationToken), `async-trait`, `serde`/`serde_yaml`, `thiserror`, `tracing`. Toolchain mirrors `../rusternetes` (`cargo fmt`, `clippy -D warnings`, `make pre-commit`).

---

## File Structure

This plan creates the workspace and three crates. Other crates (`supervisor`, `sequencer`, `config`, `platform`, `apiserver`, `controllers`, `machinectl`, `machined`) are added by the M1 plan.

```
machined-rs/
├── Cargo.toml                      # workspace manifest (members: common, resources, runtime-core)
├── rust-toolchain.toml             # pin stable, matching rusternetes
├── Makefile                        # pre-commit target (fmt + clippy + test)
├── .gitignore
├── crates/
│   ├── common/
│   │   ├── Cargo.toml
│   │   └── src/lib.rs              # tracing init + prelude re-exports
│   ├── resources/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs              # re-exports
│   │       ├── metadata.rs         # Metadata, Phase, ResourceType, Key
│   │       └── resource.rs         # Resource enum + specs + ResourceObject
│   └── runtime-core/
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs              # re-exports
│           ├── error.rs            # runtime_core::Error
│           ├── state.rs            # State store (get/list/create/update/destroy/finalizers/teardown)
│           ├── watch.rs            # Event, EventKind, broadcast plumbing
│           └── runtime.rs          # Input/Output, Controller trait, ReconcileCtx, Runtime
```

**Responsibilities:**
- `common` — process-wide `tracing` setup and a tiny prelude. No domain logic.
- `resources` — pure data: metadata, the closed `Resource` enum, and `ResourceObject` (metadata + spec). No I/O, no async.
- `runtime-core` — the store, watch bus, and controller runtime. Depends on `resources` + `common`.

---

## Task 1: Workspace scaffold + tooling

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `Makefile`
- Create: `.gitignore`
- Create: `crates/common/Cargo.toml`
- Create: `crates/common/src/lib.rs`

- [ ] **Step 1: Create the workspace manifest**

Create `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
    "crates/common",
    "crates/resources",
    "crates/runtime-core",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "Apache-2.0"

[workspace.dependencies]
tokio = { version = "1.40", features = ["full"] }
tokio-util = { version = "0.7", features = ["rt"] }
async-trait = "0.1"
serde = { version = "1.0", features = ["derive"] }
serde_yaml = "0.9"
thiserror = "1.0"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# internal crates
machined-common = { path = "crates/common" }
machined-resources = { path = "crates/resources" }
machined-runtime-core = { path = "crates/runtime-core" }
```

- [ ] **Step 2: Pin the toolchain**

Create `rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 3: Create the Makefile**

Create `Makefile`:

```make
.PHONY: pre-commit fmt clippy test

pre-commit: fmt clippy test

fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --workspace
```

- [ ] **Step 4: Create .gitignore**

Create `.gitignore`:

```gitignore
/target
**/*.rs.bk
Cargo.lock
```

> Note: `Cargo.lock` is ignored because the workspace currently ships only libraries. The M1 plan, which adds the `machined` binary, removes `Cargo.lock` from `.gitignore` and commits it.

- [ ] **Step 5: Create the `common` crate**

Create `crates/common/Cargo.toml`:

```toml
[package]
name = "machined-common"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
tracing.workspace = true
tracing-subscriber.workspace = true
```

Create `crates/common/src/lib.rs`:

```rust
//! Shared process-wide helpers for machined-rs.

use tracing_subscriber::EnvFilter;

/// Initialise structured logging to the console.
///
/// Reads the `RUST_LOG` env filter; defaults to `info` when unset. Safe to call
/// once at process start. Calling twice is a no-op (the second `try_init` fails
/// and is ignored).
pub fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
```

- [ ] **Step 6: Verify the workspace builds**

Run: `cargo build --workspace`
Expected: PASS — compiles `machined-common` with no errors. (`resources` and `runtime-core` members do not exist yet, so temporarily comment them out of `members` for this step, OR create them as empty in Task 2 first. To keep this task self-contained, edit `Cargo.toml` `members` to list only `"crates/common"` for now; Task 2 Step 1 restores the other two.)

Adjust `Cargo.toml` `members` to:

```toml
members = [
    "crates/common",
]
```

Run again: `cargo build --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml rust-toolchain.toml Makefile .gitignore crates/common
git commit -m "chore: scaffold machined-rs workspace + common crate"
```

---

## Task 2: `resources` crate — metadata + Resource enum

**Files:**
- Modify: `Cargo.toml` (restore members)
- Create: `crates/resources/Cargo.toml`
- Create: `crates/resources/src/lib.rs`
- Create: `crates/resources/src/metadata.rs`
- Create: `crates/resources/src/resource.rs`
- Test: inline `#[cfg(test)]` in `resource.rs`

- [ ] **Step 1: Restore workspace members**

Edit `Cargo.toml` `members` back to all three:

```toml
members = [
    "crates/common",
    "crates/resources",
    "crates/runtime-core",
]
```

- [ ] **Step 2: Create the resources crate manifest**

Create `crates/resources/Cargo.toml`:

```toml
[package]
name = "machined-resources"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
serde.workspace = true
```

- [ ] **Step 3: Write the failing test for metadata + resource construction**

Create `crates/resources/src/metadata.rs`:

```rust
//! Resource metadata: the COSI-style identity and lifecycle fields every
//! resource carries, independent of its typed spec.

use std::fmt;

/// The closed set of resource types known to machined-rs.
///
/// Adding a variant here forces every exhaustive match across the codebase to
/// handle it — the deliberate trade vs an open, dynamically-typed registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceType {
    MachineConfig,
    ServiceStatus,
}

impl fmt::Display for ResourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ResourceType::MachineConfig => "MachineConfig",
            ResourceType::ServiceStatus => "ServiceStatus",
        };
        f.write_str(s)
    }
}

/// Lifecycle phase of a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Normal steady state.
    Running,
    /// Marked for deletion; held while finalizers remain.
    TearingDown,
}

/// Fully-qualifying identity of a resource within the store.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub namespace: String,
    pub typ: ResourceType,
    pub id: String,
}

impl Key {
    pub fn new(
        namespace: impl Into<String>,
        typ: ResourceType,
        id: impl Into<String>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            typ,
            id: id.into(),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.namespace, self.typ, self.id)
    }
}

/// Metadata carried by every resource object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub namespace: String,
    pub typ: ResourceType,
    pub id: String,
    /// Monotonic per-resource version, bumped on every spec mutation.
    pub version: u64,
    /// Owning controller name, if this resource is controller-managed.
    pub owner: Option<String>,
    /// Finalizer names that must be cleared before deletion completes.
    pub finalizers: Vec<String>,
    pub phase: Phase,
}

impl Metadata {
    /// Construct metadata for a freshly-created resource (version 0, Running).
    pub fn new(namespace: impl Into<String>, typ: ResourceType, id: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            typ,
            id: id.into(),
            version: 0,
            owner: None,
            finalizers: Vec::new(),
            phase: Phase::Running,
        }
    }

    pub fn key(&self) -> Key {
        Key::new(self.namespace.clone(), self.typ, self.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_new_defaults() {
        let m = Metadata::new("runtime", ResourceType::ServiceStatus, "etcd");
        assert_eq!(m.version, 0);
        assert_eq!(m.phase, Phase::Running);
        assert!(m.finalizers.is_empty());
        assert!(m.owner.is_none());
        assert_eq!(m.key().to_string(), "runtime/ServiceStatus/etcd");
    }
}
```

- [ ] **Step 4: Run the metadata test to verify it fails**

Run: `cargo test -p machined-resources metadata_new_defaults`
Expected: FAIL — `resources/src/lib.rs` does not yet exist / module not declared, compile error.

- [ ] **Step 5: Create the Resource enum and ResourceObject**

Create `crates/resources/src/resource.rs`:

```rust
//! The closed `Resource` enum (typed specs) and `ResourceObject`
//! (metadata + spec) stored by the runtime.

use crate::metadata::{Metadata, ResourceType};

/// Spec for the loaded machine configuration, surfaced as a resource so
/// controllers reconcile against it via the normal watch path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineConfigSpec {
    /// Raw single-document YAML the config was parsed from. The typed view
    /// lives in the `config` crate (added in M1); runtime-core only needs to
    /// store and version the document.
    pub raw_yaml: String,
}

/// Observed state of a supervised service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceStatusSpec {
    pub service_id: String,
    pub state: ServiceState,
    pub healthy: bool,
    pub last_message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    Preparing,
    Running,
    Finished,
    Skipped,
    Failed,
}

/// The closed set of resource specs. Each variant's payload is its typed spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resource {
    MachineConfig(MachineConfigSpec),
    ServiceStatus(ServiceStatusSpec),
}

impl Resource {
    /// The `ResourceType` discriminant for this spec.
    pub fn resource_type(&self) -> ResourceType {
        match self {
            Resource::MachineConfig(_) => ResourceType::MachineConfig,
            Resource::ServiceStatus(_) => ResourceType::ServiceStatus,
        }
    }
}

/// A stored object: identity/lifecycle metadata plus its typed spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceObject {
    pub metadata: Metadata,
    pub spec: Resource,
}

impl ResourceObject {
    /// Build a fresh object in namespace `ns` with id `id` from `spec`.
    /// The metadata's type is taken from the spec, guaranteeing they agree.
    pub fn new(ns: impl Into<String>, id: impl Into<String>, spec: Resource) -> Self {
        let typ = spec.resource_type();
        Self {
            metadata: Metadata::new(ns, typ, id),
            spec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_object_type_matches_spec() {
        let obj = ResourceObject::new(
            "runtime",
            "etcd",
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: "ok".into(),
            }),
        );
        assert_eq!(obj.metadata.typ, ResourceType::ServiceStatus);
        assert_eq!(obj.spec.resource_type(), ResourceType::ServiceStatus);
    }
}
```

- [ ] **Step 6: Wire up the crate root**

Create `crates/resources/src/lib.rs`:

```rust
//! Pure resource data model for machined-rs: metadata and the closed
//! `Resource` enum. No I/O, no async.

pub mod metadata;
pub mod resource;

pub use metadata::{Key, Metadata, Phase, ResourceType};
pub use resource::{
    MachineConfigSpec, Resource, ResourceObject, ServiceState, ServiceStatusSpec,
};
```

- [ ] **Step 7: Run the resources tests to verify they pass**

Run: `cargo test -p machined-resources`
Expected: PASS — both `metadata_new_defaults` and `resource_object_type_matches_spec` pass.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/resources
git commit -m "feat(resources): metadata + closed Resource enum"
```

---

## Task 3: `runtime-core` error type + crate skeleton

**Files:**
- Create: `crates/runtime-core/Cargo.toml`
- Create: `crates/runtime-core/src/lib.rs`
- Create: `crates/runtime-core/src/error.rs`

- [ ] **Step 1: Create the runtime-core manifest**

Create `crates/runtime-core/Cargo.toml`:

```toml
[package]
name = "machined-runtime-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
machined-common.workspace = true
machined-resources.workspace = true
tokio.workspace = true
tokio-util.workspace = true
async-trait.workspace = true
thiserror.workspace = true
tracing.workspace = true
```

- [ ] **Step 2: Write the error type**

Create `crates/runtime-core/src/error.rs`:

```rust
//! Error type for the runtime-core store and controller runtime.

use machined_resources::Key;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("resource not found: {0}")]
    NotFound(Key),

    #[error("resource already exists: {0}")]
    AlreadyExists(Key),

    #[error("version conflict on {key}: expected {expected}, found {found}")]
    Conflict { key: Key, expected: u64, found: u64 },

    #[error("cannot destroy {0}: finalizers still present")]
    HasFinalizers(Key),

    #[error("controller error: {0}")]
    Controller(String),
}

pub type Result<T> = std::result::Result<T, Error>;
```

- [ ] **Step 3: Write the crate root (modules declared, filled by later tasks)**

Create `crates/runtime-core/src/lib.rs`:

```rust
//! Generic, statically-typed reconcile runtime for machined-rs.
//!
//! Provides an in-memory resource [`State`] store with COSI semantics
//! (versioned CAS updates, finalizers, owner refs, teardown), a broadcast
//! watch bus, and a [`Runtime`] that drives one reconcile loop per
//! registered [`Controller`].

pub mod error;
pub mod runtime;
pub mod state;
pub mod watch;

pub use error::{Error, Result};
pub use runtime::{Controller, Input, InputKind, Output, OutputKind, ReconcileCtx, Runtime};
pub use state::State;
pub use watch::{Event, EventKind};
```

> This will not compile until Tasks 4–6 create `state.rs`, `watch.rs`, and `runtime.rs`. That is expected; do not run a full build until Step 4 of Task 6. To keep the crate compiling between tasks, create empty placeholder files now:

Create `crates/runtime-core/src/watch.rs`:

```rust
// Filled in by Task 4.
```

Create `crates/runtime-core/src/state.rs`:

```rust
// Filled in by Task 5.
```

Create `crates/runtime-core/src/runtime.rs`:

```rust
// Filled in by Task 6.
```

Temporarily trim `lib.rs` `pub use` lines for symbols that do not exist yet — comment out the `runtime::` and `watch::`/`state::` re-exports, leaving only `pub mod` declarations and `pub use error::{Error, Result};`. Each later task restores its own re-export line.

```rust
pub mod error;
pub mod runtime;
pub mod state;
pub mod watch;

pub use error::{Error, Result};
// pub use runtime::{...};  // restored in Task 6
// pub use state::State;    // restored in Task 5
// pub use watch::{...};    // restored in Task 4
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p machined-runtime-core`
Expected: PASS — empty modules + error type compile.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime-core
git commit -m "feat(runtime-core): crate skeleton + error type"
```

---

## Task 4: Watch bus — events + broadcast

**Files:**
- Modify: `crates/runtime-core/src/watch.rs`
- Modify: `crates/runtime-core/src/lib.rs` (restore watch re-export)

- [ ] **Step 1: Write the failing test**

Replace `crates/runtime-core/src/watch.rs` with:

```rust
//! Change events emitted by the [`crate::State`] store and the broadcast
//! channel controllers subscribe to.

use machined_resources::{ResourceObject, ResourceType};
use tokio::sync::broadcast;

/// What happened to a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Created,
    Updated,
    Destroyed,
}

/// A change notification carrying the affected object's post-change state
/// (for `Destroyed`, the object as it was immediately before removal).
#[derive(Clone, Debug)]
pub struct Event {
    pub kind: EventKind,
    pub object: ResourceObject,
}

impl Event {
    pub fn namespace(&self) -> &str {
        &self.object.metadata.namespace
    }

    pub fn resource_type(&self) -> ResourceType {
        self.object.metadata.typ
    }
}

/// Capacity of the per-store broadcast channel. Sized generously; controllers
/// that lag past this see a `RecvError::Lagged` and perform a full re-list,
/// which is the correct recovery for a reconcile loop.
pub(crate) const CHANNEL_CAPACITY: usize = 1024;

/// Create the store's broadcast sender.
pub(crate) fn channel() -> broadcast::Sender<Event> {
    broadcast::Sender::new(CHANNEL_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{Resource, ServiceState, ServiceStatusSpec};

    fn sample() -> ResourceObject {
        ResourceObject::new(
            "runtime",
            "etcd",
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: "ok".into(),
            }),
        )
    }

    #[tokio::test]
    async fn broadcast_delivers_event() {
        let tx = channel();
        let mut rx = tx.subscribe();
        tx.send(Event {
            kind: EventKind::Created,
            object: sample(),
        })
        .unwrap();

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, EventKind::Created);
        assert_eq!(ev.resource_type(), ResourceType::ServiceStatus);
        assert_eq!(ev.namespace(), "runtime");
    }
}
```

- [ ] **Step 2: Restore the watch re-export**

In `crates/runtime-core/src/lib.rs`, replace the commented watch line with:

```rust
pub use watch::{Event, EventKind};
```

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p machined-runtime-core watch::tests::broadcast_delivers_event`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/runtime-core/src/watch.rs crates/runtime-core/src/lib.rs
git commit -m "feat(runtime-core): watch bus events + broadcast channel"
```

---

## Task 5: State store — CRUD + CAS + finalizers + teardown

**Files:**
- Modify: `crates/runtime-core/src/state.rs`
- Modify: `crates/runtime-core/src/lib.rs` (restore state re-export)

- [ ] **Step 1: Write the failing tests**

Replace `crates/runtime-core/src/state.rs` with:

```rust
//! In-memory resource store with COSI semantics: versioned CAS updates,
//! finalizers, owner refs, teardown, and change broadcasting.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use machined_resources::{Key, Phase, Resource, ResourceObject, ResourceType};
use tokio::sync::broadcast;

use crate::error::{Error, Result};
use crate::watch::{channel, Event, EventKind};

#[derive(Default)]
struct Inner {
    objects: HashMap<Key, ResourceObject>,
}

/// Cheap-to-clone shared handle to the resource store.
#[derive(Clone)]
pub struct State {
    inner: Arc<Mutex<Inner>>,
    tx: broadcast::Sender<Event>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            tx: channel(),
        }
    }

    /// Subscribe to all change events. Controllers filter by type/namespace.
    pub fn watch(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    fn emit(&self, kind: EventKind, object: ResourceObject) {
        // A send error means no subscribers; that is fine.
        let _ = self.tx.send(Event { kind, object });
    }

    /// Fetch a resource, or `Error::NotFound`.
    pub fn get(&self, key: &Key) -> Result<ResourceObject> {
        let inner = self.inner.lock().unwrap();
        inner
            .objects
            .get(key)
            .cloned()
            .ok_or_else(|| Error::NotFound(key.clone()))
    }

    /// List all resources of a type within a namespace.
    pub fn list(&self, namespace: &str, typ: ResourceType) -> Vec<ResourceObject> {
        let inner = self.inner.lock().unwrap();
        inner
            .objects
            .values()
            .filter(|o| o.metadata.namespace == namespace && o.metadata.typ == typ)
            .cloned()
            .collect()
    }

    /// Create a new resource. The object's version is reset to 1.
    /// Errors with `AlreadyExists` if the key is taken.
    pub fn create(&self, mut object: ResourceObject) -> Result<()> {
        let key = object.metadata.key();
        let mut inner = self.inner.lock().unwrap();
        if inner.objects.contains_key(&key) {
            return Err(Error::AlreadyExists(key));
        }
        object.metadata.version = 1;
        object.metadata.phase = Phase::Running;
        inner.objects.insert(key, object.clone());
        drop(inner);
        self.emit(EventKind::Created, object);
        Ok(())
    }

    /// Replace a resource's spec, requiring `expected_version` to match
    /// (optimistic concurrency). On success the stored version is bumped.
    pub fn update(&self, key: &Key, expected_version: u64, spec: Resource) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get_mut(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        if obj.metadata.version != expected_version {
            return Err(Error::Conflict {
                key: key.clone(),
                expected: expected_version,
                found: obj.metadata.version,
            });
        }
        obj.spec = spec;
        obj.metadata.version += 1;
        let snapshot = obj.clone();
        drop(inner);
        self.emit(EventKind::Updated, snapshot);
        Ok(())
    }

    /// Add a finalizer to a resource. Idempotent.
    pub fn add_finalizer(&self, key: &Key, finalizer: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get_mut(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        if !obj.metadata.finalizers.iter().any(|f| f == finalizer) {
            obj.metadata.finalizers.push(finalizer.to_string());
            obj.metadata.version += 1;
            let snapshot = obj.clone();
            drop(inner);
            self.emit(EventKind::Updated, snapshot);
        }
        Ok(())
    }

    /// Remove a finalizer. Idempotent.
    pub fn remove_finalizer(&self, key: &Key, finalizer: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get_mut(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        let before = obj.metadata.finalizers.len();
        obj.metadata.finalizers.retain(|f| f != finalizer);
        if obj.metadata.finalizers.len() != before {
            obj.metadata.version += 1;
            let snapshot = obj.clone();
            drop(inner);
            self.emit(EventKind::Updated, snapshot);
        }
        Ok(())
    }

    /// Mark a resource for deletion. Sets `Phase::TearingDown` and returns
    /// `true` when it is ready to destroy (no finalizers remain). When
    /// finalizers are present, the resource stays in the store in
    /// `TearingDown` so its owner's strong-input controllers can clean up.
    pub fn teardown(&self, key: &Key) -> Result<bool> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get_mut(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        if obj.metadata.phase != Phase::TearingDown {
            obj.metadata.phase = Phase::TearingDown;
            obj.metadata.version += 1;
            let snapshot = obj.clone();
            self.emit_locked(&mut inner, EventKind::Updated, snapshot);
        }
        Ok(obj.metadata.finalizers.is_empty())
    }

    // Helper that emits while already holding the lock by cloning the sender.
    fn emit_locked(&self, _inner: &mut Inner, kind: EventKind, object: ResourceObject) {
        let _ = self.tx.send(Event { kind, object });
    }

    /// Permanently remove a resource. Requires `expected_version` to match and
    /// errors with `HasFinalizers` if any finalizer remains.
    pub fn destroy(&self, key: &Key, expected_version: u64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let obj = inner
            .objects
            .get(key)
            .ok_or_else(|| Error::NotFound(key.clone()))?;
        if obj.metadata.version != expected_version {
            return Err(Error::Conflict {
                key: key.clone(),
                expected: expected_version,
                found: obj.metadata.version,
            });
        }
        if !obj.metadata.finalizers.is_empty() {
            return Err(Error::HasFinalizers(key.clone()));
        }
        let removed = inner.objects.remove(key).unwrap();
        drop(inner);
        self.emit(EventKind::Destroyed, removed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{ServiceState, ServiceStatusSpec};

    fn svc(id: &str, state: ServiceState) -> ResourceObject {
        ResourceObject::new(
            "runtime",
            id,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: id.into(),
                state,
                healthy: true,
                last_message: String::new(),
            }),
        )
    }

    fn key(id: &str) -> Key {
        Key::new("runtime", ResourceType::ServiceStatus, id)
    }

    #[test]
    fn create_sets_version_one_and_rejects_duplicates() {
        let st = State::new();
        st.create(svc("etcd", ServiceState::Preparing)).unwrap();
        let got = st.get(&key("etcd")).unwrap();
        assert_eq!(got.metadata.version, 1);

        let err = st.create(svc("etcd", ServiceState::Preparing)).unwrap_err();
        assert!(matches!(err, Error::AlreadyExists(_)));
    }

    #[test]
    fn update_requires_matching_version() {
        let st = State::new();
        st.create(svc("etcd", ServiceState::Preparing)).unwrap();

        // Stale version is rejected.
        let stale = st.update(
            &key("etcd"),
            99,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        );
        assert!(matches!(stale, Err(Error::Conflict { .. })));

        // Correct version succeeds and bumps to 2.
        st.update(
            &key("etcd"),
            1,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        )
        .unwrap();
        assert_eq!(st.get(&key("etcd")).unwrap().metadata.version, 2);
    }

    #[test]
    fn finalizer_gated_teardown() {
        let st = State::new();
        st.create(svc("etcd", ServiceState::Running)).unwrap();
        st.add_finalizer(&key("etcd"), "controller-x").unwrap();

        // teardown holds the resource while a finalizer remains.
        assert!(!st.teardown(&key("etcd")).unwrap());
        let v = st.get(&key("etcd")).unwrap().metadata.version;
        let destroy_err = st.destroy(&key("etcd"), v).unwrap_err();
        assert!(matches!(destroy_err, Error::HasFinalizers(_)));

        // Once the finalizer is cleared, teardown reports ready and destroy works.
        st.remove_finalizer(&key("etcd"), "controller-x").unwrap();
        assert!(st.teardown(&key("etcd")).unwrap());
        let v = st.get(&key("etcd")).unwrap().metadata.version;
        st.destroy(&key("etcd"), v).unwrap();
        assert!(matches!(st.get(&key("etcd")), Err(Error::NotFound(_))));
    }

    #[test]
    fn list_filters_by_namespace_and_type() {
        let st = State::new();
        st.create(svc("etcd", ServiceState::Running)).unwrap();
        st.create(svc("kubelet", ServiceState::Running)).unwrap();
        let all = st.list("runtime", ResourceType::ServiceStatus);
        assert_eq!(all.len(), 2);
        assert!(st.list("other", ResourceType::ServiceStatus).is_empty());
    }
}
```

- [ ] **Step 2: Restore the state re-export**

In `crates/runtime-core/src/lib.rs`, replace the commented state line with:

```rust
pub use state::State;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p machined-runtime-core state::`
Expected: PASS — all four state tests pass.

- [ ] **Step 4: Lint, then commit**

Run: `cargo clippy -p machined-runtime-core --all-targets -- -D warnings`
Expected: PASS (no warnings).

```bash
git add crates/runtime-core/src/state.rs crates/runtime-core/src/lib.rs
git commit -m "feat(runtime-core): State store with CAS, finalizers, teardown"
```

---

## Task 6: Controller trait + Runtime + reconcile loop

**Files:**
- Modify: `crates/runtime-core/src/runtime.rs`
- Modify: `crates/runtime-core/src/lib.rs` (restore runtime re-export)

- [ ] **Step 1: Write the failing integration test (a toy controller)**

Replace `crates/runtime-core/src/runtime.rs` with:

```rust
//! Controller abstraction and the [`Runtime`] that drives reconcile loops.

use std::time::Duration;

use async_trait::async_trait;
use machined_resources::ResourceType;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::error::Result;
use crate::state::State;
use crate::watch::Event;

/// Dependency strength of a controller input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// Depends-on: the controller is notified of teardown via finalizers.
    Strong,
    /// Watch-only: changes wake the controller but imply no ownership.
    Weak,
}

/// Ownership of a controller output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    /// Exactly one controller writes this type.
    Exclusive,
    /// Multiple controllers create objects of this type, each owning its own.
    Shared,
}

/// A declared input: a resource type (in a namespace) the controller watches.
#[derive(Clone, Debug)]
pub struct Input {
    pub namespace: String,
    pub typ: ResourceType,
    pub kind: InputKind,
}

/// A declared output: a resource type the controller writes.
#[derive(Clone, Debug)]
pub struct Output {
    pub typ: ResourceType,
    pub kind: OutputKind,
}

/// Context handed to a controller on each reconcile: the shared store.
pub struct ReconcileCtx {
    pub state: State,
}

/// A single-purpose reconciler. `reconcile` is called once at startup and
/// again whenever any declared input changes.
#[async_trait]
pub trait Controller: Send {
    fn name(&self) -> &str;
    fn inputs(&self) -> Vec<Input>;
    fn outputs(&self) -> Vec<Output>;
    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> Result<()>;
}

/// Registers controllers and drives their reconcile loops over a shared store.
pub struct Runtime {
    state: State,
    controllers: Vec<Box<dyn Controller>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            state: State::new(),
            controllers: Vec::new(),
        }
    }

    /// Build a runtime over an existing store (so callers can pre-seed it or
    /// share it with the API server).
    pub fn with_state(state: State) -> Self {
        Self {
            state,
            controllers: Vec::new(),
        }
    }

    pub fn state(&self) -> State {
        self.state.clone()
    }

    pub fn register(&mut self, controller: Box<dyn Controller>) {
        self.controllers.push(controller);
    }

    /// Spawn one reconcile loop per controller and run until `shutdown` fires.
    /// Returns when all loops have stopped.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let mut handles = Vec::new();
        for controller in self.controllers {
            let state = self.state.clone();
            let token = shutdown.clone();
            handles.push(tokio::spawn(controller_loop(controller, state, token)));
        }
        for h in handles {
            if let Err(e) = h.await {
                error!("controller task panicked: {e}");
            }
        }
        Ok(())
    }
}

/// Debounce window: after a wake, drain any immediately-pending events before
/// reconciling so a burst collapses into one reconcile pass.
const DEBOUNCE: Duration = Duration::from_millis(20);

async fn controller_loop(
    mut controller: Box<dyn Controller>,
    state: State,
    shutdown: CancellationToken,
) {
    let ctx = ReconcileCtx {
        state: state.clone(),
    };
    let inputs = controller.inputs();
    let mut rx = state.watch();

    // Initial reconcile.
    reconcile_once(&mut controller, &ctx).await;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                return;
            }
            recv = rx.recv() => {
                match recv {
                    Ok(event) => {
                        if !matches_inputs(&inputs, &event) {
                            continue;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(controller = controller.name(), skipped = n, "watch lagged; forcing reconcile");
                        // Fall through to reconcile — a full re-list is the cure.
                    }
                    Err(RecvError::Closed) => return,
                }
                // Debounce: collapse a burst into a single reconcile.
                tokio::time::sleep(DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
                reconcile_once(&mut controller, &ctx).await;
            }
        }
    }
}

fn matches_inputs(inputs: &[Input], event: &Event) -> bool {
    inputs.iter().any(|i| {
        i.typ == event.resource_type() && i.namespace == event.namespace()
    })
}

async fn reconcile_once(controller: &mut Box<dyn Controller>, ctx: &ReconcileCtx) {
    if let Err(e) = controller.reconcile(ctx).await {
        error!(controller = controller.name(), error = %e, "reconcile failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{
        Key, Resource, ResourceObject, ServiceState, ServiceStatusSpec,
    };

    /// A toy controller: for every ServiceStatus in `Failed` state, it records
    /// a finalizer-free marker by flipping `healthy` to false via update.
    struct HealthMarker;

    #[async_trait]
    impl Controller for HealthMarker {
        fn name(&self) -> &str {
            "health-marker"
        }
        fn inputs(&self) -> Vec<Input> {
            vec![Input {
                namespace: "runtime".into(),
                typ: ResourceType::ServiceStatus,
                kind: InputKind::Weak,
            }]
        }
        fn outputs(&self) -> Vec<Output> {
            vec![Output {
                typ: ResourceType::ServiceStatus,
                kind: OutputKind::Exclusive,
            }]
        }
        async fn reconcile(&mut self, ctx: &ReconcileCtx) -> Result<()> {
            for obj in ctx.state.list("runtime", ResourceType::ServiceStatus) {
                if let Resource::ServiceStatus(ref s) = obj.spec {
                    if s.state == ServiceState::Failed && s.healthy {
                        let mut new = s.clone();
                        new.healthy = false;
                        // Ignore conflicts; a later event re-reconciles.
                        let _ = ctx.state.update(
                            &obj.metadata.key(),
                            obj.metadata.version,
                            Resource::ServiceStatus(new),
                        );
                    }
                }
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn controller_reacts_to_input_change() {
        let mut rt = Runtime::new();
        let state = rt.state();
        rt.register(Box::new(HealthMarker));

        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let handle = tokio::spawn(async move { rt.run(token).await });

        // Seed a failed service after the runtime is up.
        state
            .create(ResourceObject::new(
                "runtime",
                "etcd",
                Resource::ServiceStatus(ServiceStatusSpec {
                    service_id: "etcd".into(),
                    state: ServiceState::Failed,
                    healthy: true,
                    last_message: "boom".into(),
                }),
            ))
            .unwrap();

        // Poll until the controller has flipped healthy=false.
        let key = Key::new("runtime", ResourceType::ServiceStatus, "etcd");
        let mut flipped = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Ok(obj) = state.get(&key) {
                if let Resource::ServiceStatus(s) = obj.spec {
                    if !s.healthy {
                        flipped = true;
                        break;
                    }
                }
            }
        }
        assert!(flipped, "controller never reconciled the failed service");

        shutdown.cancel();
        handle.await.unwrap().unwrap();
    }
}
```

- [ ] **Step 2: Restore the runtime re-export**

In `crates/runtime-core/src/lib.rs`, replace the commented runtime line with:

```rust
pub use runtime::{Controller, Input, InputKind, Output, OutputKind, ReconcileCtx, Runtime};
```

The final `lib.rs` should now read:

```rust
//! Generic, statically-typed reconcile runtime for machined-rs.
//!
//! Provides an in-memory resource [`State`] store with COSI semantics
//! (versioned CAS updates, finalizers, owner refs, teardown), a broadcast
//! watch bus, and a [`Runtime`] that drives one reconcile loop per
//! registered [`Controller`].

pub mod error;
pub mod runtime;
pub mod state;
pub mod watch;

pub use error::{Error, Result};
pub use runtime::{Controller, Input, InputKind, Output, OutputKind, ReconcileCtx, Runtime};
pub use state::State;
pub use watch::{Event, EventKind};
```

- [ ] **Step 3: Run the integration test to verify it passes**

Run: `cargo test -p machined-runtime-core runtime::tests::controller_reacts_to_input_change`
Expected: PASS — the controller flips `healthy` within the poll window.

- [ ] **Step 4: Full build + lint + test**

Run: `cargo build --workspace`
Expected: PASS.

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS (no warnings).

Run: `cargo test --workspace`
Expected: PASS — all `resources` and `runtime-core` tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime-core/src/runtime.rs crates/runtime-core/src/lib.rs
git commit -m "feat(runtime-core): Controller trait + Runtime reconcile loop"
```

---

## Task 7: Doc + CI polish

**Files:**
- Create: `README.md`
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: Write a minimal README**

Create `README.md`:

```markdown
# machined-rs

A generic, immutable Rust node-OS daemon (PID 1 / init + machine management),
inspired by Talos Linux's `machined`. It boots a node, configures it, and
supervises a config-declared workload payload (e.g. a Kubernetes distribution).
Workload- and distro-agnostic; [rusternetes](../rusternetes) is the reference
payload, not a dependency.

## Status

Milestone M0 (the `runtime-core` reconcile foundation) — in progress.
See `docs/superpowers/specs/` and `docs/superpowers/plans/`.

## Build

```bash
cargo build --workspace
make pre-commit   # fmt + clippy -D warnings + test
```
```

- [ ] **Step 2: Add CI**

Create `.github/workflows/ci.yml`:

```yaml
name: ci
on:
  push:
    branches: [main]
  pull_request:

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - name: fmt
        run: cargo fmt --all -- --check
      - name: clippy
        run: cargo clippy --all-targets --all-features -- -D warnings
      - name: test
        run: cargo test --workspace
```

- [ ] **Step 3: Verify formatting is clean**

Run: `cargo fmt --all -- --check`
Expected: PASS (no diff). If it fails, run `cargo fmt --all` and re-check.

- [ ] **Step 4: Commit**

```bash
git add README.md .github/workflows/ci.yml
git commit -m "chore: README + CI workflow"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage (M0 deliverables):** workspace scaffold + `make pre-commit` (Task 1, 7) ✓; `common` logging (Task 1) ✓; `Resource` enum + `Metadata` + `Versioned`-equivalent via `version` field (Task 2) ✓; `State` get/list/create/update-CAS/destroy + finalizers + owner field (Task 2 metadata, Task 5) ✓; broadcast watch with type/namespace filtering (Task 4 + `matches_inputs` in Task 6) ✓; `Controller` trait + `Runtime` spawning loops + dependency wiring via declared inputs (Task 6) ✓; unit tests for CAS conflict, watch delivery, finalizer-gated teardown, toy controller (Tasks 4–6) ✓.
- **Note on owner refs:** M0 stores `owner: Option<String>` on metadata and `Output`/`InputKind` carry the strong/weak + exclusive/shared semantics, but automatic owner-driven cascading teardown is **not** wired in M0 — controllers manage finalizers explicitly. Full owner-cascade lands when the first strong-input controller pair exists (M2). This is intentional scope-limiting, recorded here so the M2 plan picks it up.
- **Deferred:** real resource types beyond `MachineConfig`/`ServiceStatus` arrive with their subsystems (M1+).
