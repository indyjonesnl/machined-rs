# machined-rs M4a — Container Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M0–M3 merged to `main`. Work on branch `spec/machined-rs-m4a-runtime`.

**Goal:** containerd runs as a built-in supervised system service and machined continuously verifies real CRI readiness over the containerd unix socket, publishing `RuntimeStatus` (visible via `machinectl get RuntimeStatus`).

**Architecture:** A `cri` leaf crate holds a trimmed CRI proto (`Version`+`Status`, field numbers verbatim from upstream `cri-api`), a `CriClient` trait, a tonic-over-UDS `GrpcCriClient`, and a fake. Pure helpers in `config` build the injected containerd `ServiceConfig` + minimal `config.toml`; the sequencer's StartServices task prepends the built-in service. A periodic `RuntimeHealthController` probes CRI and publishes `RuntimeStatus`.

**Tech Stack:** tonic 0.12 over `UnixStream` (`connect_with_connector` + `tower::service_fn` + `hyper_util::rt::TokioIo`), vendored protoc (existing pattern), the M1 supervisor + M2c periodic reconcile.

---

## File Structure

```
crates/cri/
├── Cargo.toml                    # NEW
├── build.rs                      # NEW (vendored protoc, like apiserver)
├── proto/runtime.proto           # NEW: trimmed CRI (RuntimeService Version+Status)
└── src/
    ├── lib.rs                    # NEW: pb module, CriError, RuntimeVersion, CriClient trait
    ├── grpc.rs                   # NEW: GrpcCriClient (UDS connector) [SPIKE]
    └── fake.rs                   # NEW: FakeCriClient
crates/cri/tests/uds.rs           # NEW: fake CRI server on UnixListener ↔ real client; gated real-containerd test
crates/config/src/types.rs        # MODIFY: RuntimeSection (custom defaults)
crates/config/src/runtime_svc.rs  # NEW: containerd_service/_config_toml/merge + reserved-id validation
crates/config/src/{load,provider,lib}.rs  # MODIFY: validation hook + runtime() accessor + re-exports
crates/resources/src/runtime_status.rs    # NEW: RuntimeStatus
crates/resources/src/{metadata,resource,lib}.rs  # MODIFY: variant + re-export
crates/apiserver/src/mapping.rs   # MODIFY: parse + fields arms (compile-forced)
crates/controllers/src/runtime/{mod,health}.rs   # NEW: RuntimeHealthController
crates/sequencer/src/boot.rs      # MODIFY: StartServices prepends containerd + writes config.toml
crates/machined/src/main.rs       # MODIFY: register RuntimeHealthController
crates/machined/tests/runtime.rs  # NEW: e2e (fake CRI → RuntimeStatus ready)
```

---

## Task 1: `cri` crate (UDS + trimmed-proto spikes)

> **SPIKE NOTE (UDS connector):** tonic 0.12's UDS pattern is
> `Endpoint::try_from("http://[::]:50051")?.connect_with_connector(service_fn(move |_: Uri| { ... UnixStream::connect(path) ... TokioIo::new(...) }))`.
> The exact wrapper (`hyper_util::rt::TokioIo`) and the `service_fn` error type vary by tonic/hyper
> minor; adapt inside `grpc.rs` only (use the vendored tonic examples/source under
> `~/.cargo/registry`). Do NOT change the `CriClient` trait. The UDS integration test is the
> acceptance gate.
>
> **SPIKE NOTE (proto):** the trimmed `runtime.proto` below copies field numbers from upstream
> `k8s.io/cri-api` `runtime/v1/api.proto`. Trim whole messages only; never renumber. If tonic-build
> rejects something, fix syntax only — field numbers are wire-contract.

**Files:**
- Modify: `Cargo.toml` (member + deps: `tower = "0.4"`, `hyper-util = "0.1"`, `machined-cri` path)
- Create: `crates/cri/Cargo.toml`
- Create: `crates/cri/build.rs`
- Create: `crates/cri/proto/runtime.proto`
- Create: `crates/cri/src/lib.rs`
- Create: `crates/cri/src/grpc.rs`
- Create: `crates/cri/src/fake.rs`
- Create: `crates/cri/tests/uds.rs`

- [ ] **Step 1: Workspace wiring**

Root `Cargo.toml`: add `"crates/cri"` to members; to `[workspace.dependencies]` add:

```toml
tower = "0.4"
hyper-util = "0.1"

machined-cri = { path = "crates/cri" }
```

- [ ] **Step 2: Manifest + build.rs**

`crates/cri/Cargo.toml`:

```toml
[package]
name = "machined-cri"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
async-trait.workspace = true
thiserror.workspace = true
tokio.workspace = true
tonic.workspace = true
prost.workspace = true
tower.workspace = true
hyper-util.workspace = true

[build-dependencies]
tonic-build.workspace = true
protoc-bin-vendored.workspace = true

[dev-dependencies]
tokio-stream = { workspace = true, features = ["net"] }
```

`crates/cri/build.rs`:

```rust
fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);
    tonic_build::compile_protos("proto/runtime.proto").expect("compile runtime.proto");
}
```

- [ ] **Step 3: The trimmed proto**

`crates/cri/proto/runtime.proto` (field numbers verbatim from upstream cri-api v1):

```proto
syntax = "proto3";
package runtime.v1;

// Trimmed k8s CRI RuntimeService: health-relevant RPCs only. Field numbers are
// copied exactly from k8s.io/cri-api runtime/v1/api.proto (wire-compatible).
service RuntimeService {
  rpc Version(VersionRequest) returns (VersionResponse) {}
  rpc Status(StatusRequest) returns (StatusResponse) {}
}

message VersionRequest { string version = 1; }
message VersionResponse {
  string version = 1;
  string runtime_name = 2;
  string runtime_version = 3;
  string runtime_api_version = 4;
}

message StatusRequest { bool verbose = 1; }
message RuntimeCondition {
  string type = 1;
  bool status = 2;
  string reason = 3;
  string message = 4;
}
message RuntimeStatus { repeated RuntimeCondition conditions = 1; }
message StatusResponse {
  RuntimeStatus status = 1;
  map<string, string> info = 2;
}
```

- [ ] **Step 4: Trait + fake + crate root**

`crates/cri/src/lib.rs`:

```rust
//! Minimal CRI client: just enough to know the container runtime is healthy.

pub mod fake;
pub mod grpc;

/// Generated (trimmed) CRI protobuf types.
pub mod pb {
    tonic::include_proto!("runtime.v1");
}

use async_trait::async_trait;

pub use fake::FakeCriClient;
pub use grpc::GrpcCriClient;

#[derive(thiserror::Error, Debug)]
pub enum CriError {
    #[error("cri connect: {0}")]
    Connect(String),
    #[error("cri rpc: {0}")]
    Rpc(String),
}

pub type Result<T> = std::result::Result<T, CriError>;

/// Identity of the running container runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeVersion {
    pub runtime_name: String,
    pub runtime_version: String,
}

/// Health-probe view of a CRI runtime.
#[async_trait]
pub trait CriClient: Send + Sync {
    /// RuntimeService.Version.
    async fn version(&self) -> Result<RuntimeVersion>;
    /// True iff the RuntimeReady condition is true (NetworkReady not required).
    async fn ready(&self) -> Result<bool>;
}
```

`crates/cri/src/fake.rs`:

```rust
//! In-memory CRI client for tests.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{CriClient, CriError, Result, RuntimeVersion};

#[derive(Default)]
struct FakeState {
    version: Option<RuntimeVersion>,
    ready: bool,
    calls: usize,
}

#[derive(Default)]
pub struct FakeCriClient {
    state: Mutex<FakeState>,
}

impl FakeCriClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the runtime identity; absent → all calls error (unreachable).
    pub fn with_version(self, name: &str, version: &str) -> Self {
        self.state.lock().unwrap().version = Some(RuntimeVersion {
            runtime_name: name.into(),
            runtime_version: version.into(),
        });
        self
    }

    pub fn with_ready(self, ready: bool) -> Self {
        self.state.lock().unwrap().ready = ready;
        self
    }

    pub fn calls(&self) -> usize {
        self.state.lock().unwrap().calls
    }
}

#[async_trait]
impl CriClient for FakeCriClient {
    async fn version(&self) -> Result<RuntimeVersion> {
        let mut s = self.state.lock().unwrap();
        s.calls += 1;
        s.version
            .clone()
            .ok_or_else(|| CriError::Connect("unreachable".into()))
    }

    async fn ready(&self) -> Result<bool> {
        let s = self.state.lock().unwrap();
        if s.version.is_none() {
            return Err(CriError::Connect("unreachable".into()));
        }
        Ok(s.ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_round_trip() {
        let f = FakeCriClient::new().with_version("containerd", "2.0").with_ready(true);
        assert_eq!(f.version().await.unwrap().runtime_name, "containerd");
        assert!(f.ready().await.unwrap());
        assert_eq!(f.calls(), 1);

        let unreachable = FakeCriClient::new();
        assert!(unreachable.version().await.is_err());
        assert!(unreachable.ready().await.is_err());
    }
}
```

- [ ] **Step 5: The UDS gRPC client (SPIKE)**

`crates/cri/src/grpc.rs`:

```rust
//! Real CRI client: tonic gRPC over the containerd unix socket.

use std::path::PathBuf;

use async_trait::async_trait;
use hyper_util::rt::TokioIo;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use crate::pb::runtime_service_client::RuntimeServiceClient;
use crate::pb::{StatusRequest, VersionRequest};
use crate::{CriClient, CriError, Result, RuntimeVersion};

/// CRI over a unix socket. Connects per-probe (the socket may appear only after
/// containerd starts; a connect failure is the transient-unreachable case).
pub struct GrpcCriClient {
    socket: PathBuf,
}

impl GrpcCriClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    async fn connect(&self) -> Result<RuntimeServiceClient<Channel>> {
        let path = self.socket.clone();
        // The URI is ignored; the connector dials the unix socket.
        let channel = Endpoint::try_from("http://[::]:50051")
            .map_err(|e| CriError::Connect(e.to_string()))?
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|e| CriError::Connect(e.to_string()))?;
        Ok(RuntimeServiceClient::new(channel))
    }
}

#[async_trait]
impl CriClient for GrpcCriClient {
    async fn version(&self) -> Result<RuntimeVersion> {
        let mut client = self.connect().await?;
        let resp = client
            .version(VersionRequest::default())
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(RuntimeVersion {
            runtime_name: resp.runtime_name,
            runtime_version: resp.runtime_version,
        })
    }

    async fn ready(&self) -> Result<bool> {
        let mut client = self.connect().await?;
        let resp = client
            .status(StatusRequest::default())
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp
            .status
            .map(|s| {
                s.conditions
                    .iter()
                    .any(|c| c.r#type == "RuntimeReady" && c.status)
            })
            .unwrap_or(false))
    }
}
```

- [ ] **Step 6: The UDS integration test (acceptance gate) + gated real test**

`crates/cri/tests/uds.rs`:

```rust
//! Root-free: a fake CRI server on a UnixListener, probed by the real
//! GrpcCriClient — proves the UDS connector + trimmed wire format end-to-end.

use machined_cri::pb::runtime_service_server::{RuntimeService, RuntimeServiceServer};
use machined_cri::pb::{
    RuntimeCondition, RuntimeStatus, StatusRequest, StatusResponse, VersionRequest,
    VersionResponse,
};
use machined_cri::{CriClient, GrpcCriClient};
use tonic::{Request, Response, Status};

struct FakeCriServer {
    ready: bool,
}

#[tonic::async_trait]
impl RuntimeService for FakeCriServer {
    async fn version(
        &self,
        _r: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: "0.1.0".into(),
            runtime_name: "containerd".into(),
            runtime_version: "2.0.0".into(),
            runtime_api_version: "v1".into(),
        }))
    }

    async fn status(&self, _r: Request<StatusRequest>) -> Result<Response<StatusResponse>, Status> {
        Ok(Response::new(StatusResponse {
            status: Some(RuntimeStatus {
                conditions: vec![
                    RuntimeCondition {
                        r#type: "RuntimeReady".into(),
                        status: self.ready,
                        reason: String::new(),
                        message: String::new(),
                    },
                    RuntimeCondition {
                        r#type: "NetworkReady".into(),
                        status: false, // must NOT gate readiness
                        reason: String::new(),
                        message: String::new(),
                    },
                ],
            }),
            info: Default::default(),
        }))
    }
}

async fn spawn_server(ready: bool) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mnd-cri-{}-{}", std::process::id(), ready));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("cri.sock");
    std::fs::remove_file(&sock).ok();
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(RuntimeServiceServer::new(FakeCriServer { ready }))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    sock
}

#[tokio::test]
async fn probes_a_cri_server_over_uds() {
    let sock = spawn_server(true).await;
    let client = GrpcCriClient::new(&sock);
    let v = client.version().await.unwrap();
    assert_eq!(v.runtime_name, "containerd");
    assert_eq!(v.runtime_version, "2.0.0");
    assert!(client.ready().await.unwrap(), "RuntimeReady=true must be ready");
}

#[tokio::test]
async fn not_ready_when_runtime_condition_false() {
    let sock = spawn_server(false).await;
    let client = GrpcCriClient::new(&sock);
    assert!(!client.ready().await.unwrap());
}

#[tokio::test]
async fn missing_socket_is_a_connect_error() {
    let client = GrpcCriClient::new("/no/such/cri.sock");
    assert!(client.version().await.is_err());
}

#[tokio::test]
#[ignore = "requires a running containerd at /run/containerd/containerd.sock"]
async fn probes_real_containerd() {
    let client = GrpcCriClient::new("/run/containerd/containerd.sock");
    let v = client.version().await.unwrap();
    assert!(!v.runtime_name.is_empty());
}
```

- [ ] **Step 7: Build (spike gate) + test + commit**

Run: `cargo build -p machined-cri` → PASS (adapt the UDS connector per SPIKE NOTE until green; record changes).
Run: `cargo test -p machined-cri` → fake unit (1) + 3 UDS tests pass; `probes_real_containerd` ignored.
Run: `cargo clippy -p machined-cri --all-targets -- -D warnings` → clean. `cargo fmt --all` then `--check` → clean.

```bash
git add Cargo.toml Cargo.lock crates/cri
git commit -m "feat(cri): trimmed CRI client over UDS (Version/Status) + fake"
```

---

## Task 2: config `runtime` section + helpers + `RuntimeStatus` resource + apiserver arms

**Files:**
- Modify: `crates/config/src/types.rs`
- Create: `crates/config/src/runtime_svc.rs`
- Modify: `crates/config/src/{load,provider,lib}.rs`
- Modify: every `MachineSection { ... }` literal (grep — E0063 follow-through)
- Create: `crates/resources/src/runtime_status.rs`
- Modify: `crates/resources/src/{metadata,resource,lib}.rs`
- Modify: `crates/apiserver/src/mapping.rs`

- [ ] **Step 1: RuntimeSection with real defaults**

In `crates/config/src/types.rs`, add to `MachineSection` (after `time`):

```rust
    /// Container runtime (containerd) management.
    #[serde(default)]
    pub runtime: RuntimeSection,
```

Append the type:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RuntimeSection {
    /// Disable runtime management entirely.
    pub disabled: bool,
    /// containerd binary path.
    pub binary: String,
    /// CRI unix socket path.
    pub socket: String,
    /// Generated containerd config path.
    pub config_path: String,
}

impl Default for RuntimeSection {
    fn default() -> Self {
        Self {
            disabled: false,
            binary: "/usr/bin/containerd".into(),
            socket: "/run/containerd/containerd.sock".into(),
            config_path: "/etc/containerd/config.toml".into(),
        }
    }
}
```

- [ ] **Step 2: Pure runtime-service helpers + reserved-id validation**

Create `crates/config/src/runtime_svc.rs`:

```rust
//! Built-in containerd service: pure construction + validation helpers.

use crate::types::{RestartPolicy, RuntimeSection, ServiceConfig};

/// The reserved id of the machined-managed runtime service.
pub const RUNTIME_SERVICE_ID: &str = "containerd";

/// The injected containerd service definition.
pub fn containerd_service(rt: &RuntimeSection) -> ServiceConfig {
    ServiceConfig {
        id: RUNTIME_SERVICE_ID.to_string(),
        command: vec![
            rt.binary.clone(),
            "--config".to_string(),
            rt.config_path.clone(),
        ],
        depends_on: Vec::new(),
        restart: RestartPolicy::Always,
    }
}

/// Minimal CRI-enabled containerd config.
pub fn containerd_config_toml(rt: &RuntimeSection) -> String {
    format!(
        "version = 2\n[grpc]\n  address = \"{}\"\n[plugins.\"io.containerd.grpc.v1.cri\"]\n",
        rt.socket
    )
}

/// The full service list the supervisor should run: the built-in runtime first
/// (unless disabled), then the user services.
pub fn effective_services(rt: &RuntimeSection, user: &[ServiceConfig]) -> Vec<ServiceConfig> {
    let mut out = Vec::with_capacity(user.len() + 1);
    if !rt.disabled {
        out.push(containerd_service(rt));
    }
    out.extend_from_slice(user);
    out
}

/// Reject user services that collide with the reserved runtime id.
pub fn validate_services(user: &[ServiceConfig]) -> Result<(), String> {
    if user.iter().any(|s| s.id == RUNTIME_SERVICE_ID) {
        return Err(format!("service id '{RUNTIME_SERVICE_ID}' is reserved"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_containerd_service_and_toml() {
        let rt = RuntimeSection::default();
        let svc = containerd_service(&rt);
        assert_eq!(svc.id, "containerd");
        assert_eq!(
            svc.command,
            vec!["/usr/bin/containerd", "--config", "/etc/containerd/config.toml"]
        );
        assert_eq!(svc.restart, RestartPolicy::Always);
        assert!(containerd_config_toml(&rt).contains("/run/containerd/containerd.sock"));
    }

    #[test]
    fn effective_services_injects_first_unless_disabled() {
        let user = vec![ServiceConfig {
            id: "payload".into(),
            command: vec!["/bin/payload".into()],
            depends_on: vec!["containerd".into()],
            restart: Default::default(),
        }];
        let on = effective_services(&RuntimeSection::default(), &user);
        assert_eq!(on.len(), 2);
        assert_eq!(on[0].id, "containerd");

        let off = effective_services(
            &RuntimeSection {
                disabled: true,
                ..Default::default()
            },
            &user,
        );
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].id, "payload");
    }

    #[test]
    fn rejects_reserved_id() {
        let bad = vec![ServiceConfig {
            id: "containerd".into(),
            command: vec!["/bin/x".into()],
            depends_on: vec![],
            restart: Default::default(),
        }];
        assert!(validate_services(&bad).is_err());
        assert!(validate_services(&[]).is_ok());
    }
}
```

- [ ] **Step 3: Hook validation into load + accessor + re-exports**

In `crates/config/src/load.rs`, change `load_from_str` to validate:

```rust
pub fn load_from_str(yaml: &str) -> Result<MachineConfig, ConfigError> {
    let cfg: MachineConfig = serde_yaml::from_str(yaml)?;
    crate::runtime_svc::validate_services(&cfg.machine.services)
        .map_err(ConfigError::Invalid)?;
    Ok(cfg)
}
```

Add the `Invalid` variant to `ConfigError` (in whichever file defines it — `load.rs` or `lib.rs`):

```rust
    #[error("invalid config: {0}")]
    Invalid(String),
```

In `crates/config/src/provider.rs`: add `RuntimeSection` to the types import + accessor:

```rust
    pub fn runtime(&self) -> &RuntimeSection {
        &self.config.machine.runtime
    }
```

In `crates/config/src/lib.rs`: `pub mod runtime_svc;`, add `RuntimeSection` to the types re-export, and re-export the helpers:

```rust
pub use runtime_svc::{
    containerd_config_toml, containerd_service, effective_services, validate_services,
    RUNTIME_SERVICE_ID,
};
```

Add config tests to `load.rs`:

```rust
    #[test]
    fn runtime_defaults() {
        let cfg = load_from_str("machine: {}").unwrap();
        assert!(!cfg.machine.runtime.disabled);
        assert_eq!(cfg.machine.runtime.binary, "/usr/bin/containerd");
        assert_eq!(cfg.machine.runtime.socket, "/run/containerd/containerd.sock");
    }

    #[test]
    fn reserved_service_id_rejected() {
        let yaml = "machine:\n  services:\n    - id: containerd\n      command: [/bin/x]\n";
        assert!(load_from_str(yaml).is_err());
    }
```

- [ ] **Step 4: MachineSection literal follow-through**

Adding `runtime` breaks every explicit `MachineSection { ... }` literal (E0063). Run
`grep -rln 'MachineSection {' crates` and add `runtime: Default::default(),` (after `time:`) to every
literal — expect ~8 sites (sequencer boot.rs, machined tests boot_harness/network/mount/provision/time,
controllers network/config_controller.rs + block/provision.rs helpers). `cargo build --workspace`
until clean.

- [ ] **Step 5: RuntimeStatus resource + apiserver arms**

Create `crates/resources/src/runtime_status.rs`:

```rust
//! Container-runtime health resource. Pure data.

/// Observed container-runtime (CRI) health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub ready: bool,
    pub name: String,
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs() {
        let r = RuntimeStatus {
            ready: true,
            name: "containerd".into(),
            version: "2.0.0".into(),
        };
        assert!(r.ready);
    }
}
```

`crates/resources/src/metadata.rs`: add `RuntimeStatus` after `TimeStatus` (enum + Display arm).
`crates/resources/src/resource.rs`: `use crate::runtime_status::RuntimeStatus;`, variant
`RuntimeStatus(RuntimeStatus)`, `resource_type()` arm.
`crates/resources/src/lib.rs`: `pub mod runtime_status;` + `pub use runtime_status::RuntimeStatus;`.

`crates/apiserver/src/mapping.rs` (the closed enum forces both arms):

```rust
        "RuntimeStatus" => ResourceType::RuntimeStatus,
```

```rust
        Resource::RuntimeStatus(r) => vec![
            kv("ready", r.ready),
            kv("name", &r.name),
            kv("version", &r.version),
        ],
```

- [ ] **Step 6: Test + commit**

Run: `cargo test -p machined-config -p machined-resources -p machined-apiserver` → all pass (incl. the new runtime_svc + load tests).
Run: `cargo build --workspace` → PASS. clippy `-D warnings` clean. fmt clean.

```bash
git add crates/config crates/resources crates/apiserver crates/sequencer crates/machined crates/controllers
git commit -m "feat(config,resources): runtime section + containerd service helpers + RuntimeStatus"
```

---

## Task 3: `RuntimeHealthController`

**Files:**
- Modify: `crates/controllers/Cargo.toml` (+ `machined-cri.workspace = true`)
- Create: `crates/controllers/src/runtime/mod.rs`
- Create: `crates/controllers/src/runtime/health.rs`
- Modify: `crates/controllers/src/lib.rs` (`pub mod runtime;`)

- [ ] **Step 1: Module scaffolding**

`crates/controllers/src/runtime/mod.rs`:

```rust
//! Container-runtime controllers.

pub mod health;

pub use health::RuntimeHealthController;

/// Namespace for runtime resources.
pub const NS: &str = "runtime";
```

In `crates/controllers/src/lib.rs`, add `pub mod runtime;`.

- [ ] **Step 2: The controller + tests**

`crates/controllers/src/runtime/health.rs`:

```rust
//! Periodically probes the CRI socket and publishes RuntimeStatus.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use machined_config::Provider;
use machined_cri::CriClient;
use machined_resources::{Resource, ResourceObject, ResourceType, RuntimeStatus};
use machined_runtime_core::{reconcile_owned, Controller, Input, Output, OutputKind, ReconcileCtx};
use tracing::warn;

use super::NS;

const OWNER: &str = "runtime-health";

pub struct RuntimeHealthController {
    cri: Arc<dyn CriClient>,
    provider: Provider,
}

impl RuntimeHealthController {
    pub fn new(cri: Arc<dyn CriClient>, provider: Provider) -> Self {
        Self { cri, provider }
    }
}

fn status_obj(ready: bool, name: &str, version: &str) -> ResourceObject {
    ResourceObject::new(
        NS,
        "containerd",
        Resource::RuntimeStatus(RuntimeStatus {
            ready,
            name: name.to_string(),
            version: version.to_string(),
        }),
    )
}

#[async_trait]
impl Controller for RuntimeHealthController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        Vec::new()
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::RuntimeStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    fn resync_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(10))
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        if self.provider.runtime().disabled {
            reconcile_owned(
                &ctx.state,
                OWNER,
                NS,
                ResourceType::RuntimeStatus,
                vec![status_obj(false, "", "")],
            )?;
            return Ok(());
        }

        let (ready, name, version) = match (self.cri.ready().await, self.cri.version().await) {
            (Ok(ready), Ok(v)) => (ready, v.runtime_name, v.runtime_version),
            (r, v) => {
                let e = r.err().map(|e| e.to_string()).or(v.err().map(|e| e.to_string()));
                warn!(error = ?e, "cri probe failed; runtime not ready");
                (false, String::new(), String::new())
            }
        };
        reconcile_owned(
            &ctx.state,
            OWNER,
            NS,
            ResourceType::RuntimeStatus,
            vec![status_obj(ready, &name, &version)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::{MachineConfig, MachineSection, RuntimeSection};
    use machined_cri::FakeCriClient;
    use machined_resources::Key;
    use machined_runtime_core::{ReconcileCtx, State};

    fn provider(disabled: bool) -> Provider {
        Provider::new(MachineConfig {
            machine: MachineSection {
                hostname: None,
                sysctls: vec![],
                services: vec![],
                network: Default::default(),
                install: None,
                time: Default::default(),
                runtime: RuntimeSection {
                    disabled,
                    ..Default::default()
                },
            },
        })
    }

    fn runtime_status(state: &State) -> RuntimeStatus {
        match state
            .get(&Key::new(NS, ResourceType::RuntimeStatus, "containerd"))
            .unwrap()
            .spec
        {
            Resource::RuntimeStatus(r) => r,
            _ => panic!("wrong type"),
        }
    }

    #[tokio::test]
    async fn publishes_ready_runtime() {
        let cri = Arc::new(FakeCriClient::new().with_version("containerd", "2.0.0").with_ready(true));
        let state = State::new();
        let ctx = ReconcileCtx { state: state.clone() };
        let mut c = RuntimeHealthController::new(cri, provider(false));
        c.reconcile(&ctx).await.unwrap();
        let st = runtime_status(&state);
        assert!(st.ready);
        assert_eq!(st.name, "containerd");
        assert_eq!(st.version, "2.0.0");
    }

    #[tokio::test]
    async fn runtime_not_ready_is_published() {
        let cri = Arc::new(FakeCriClient::new().with_version("containerd", "2.0.0").with_ready(false));
        let state = State::new();
        let ctx = ReconcileCtx { state: state.clone() };
        let mut c = RuntimeHealthController::new(cri, provider(false));
        c.reconcile(&ctx).await.unwrap();
        assert!(!runtime_status(&state).ready);
    }

    #[tokio::test]
    async fn unreachable_is_transient_not_error() {
        let cri = Arc::new(FakeCriClient::new()); // no version → errors
        let state = State::new();
        let ctx = ReconcileCtx { state: state.clone() };
        let mut c = RuntimeHealthController::new(cri, provider(false));
        c.reconcile(&ctx).await.unwrap(); // Ok, not Err
        assert!(!runtime_status(&state).ready);
    }

    #[tokio::test]
    async fn disabled_does_not_probe() {
        let cri = Arc::new(FakeCriClient::new().with_version("containerd", "2.0.0").with_ready(true));
        let state = State::new();
        let ctx = ReconcileCtx { state: state.clone() };
        let mut c = RuntimeHealthController::new(cri.clone(), provider(true));
        c.reconcile(&ctx).await.unwrap();
        assert!(!runtime_status(&state).ready);
        assert_eq!(cri.calls(), 0);
    }
}
```

- [ ] **Step 3: Test + commit**

Run: `cargo test -p machined-controllers runtime` → 4 pass. Full crate suite passes. clippy/fmt clean.

```bash
git add crates/controllers
git commit -m "feat(controllers): RuntimeHealthController (periodic CRI probe)"
```

---

## Task 4: sequencer injection + machined wiring + e2e

**Files:**
- Modify: `crates/sequencer/src/boot.rs`
- Modify: `crates/machined/src/main.rs` (+ Cargo.toml `machined-cri`)
- Create: `crates/machined/tests/runtime.rs`

- [ ] **Step 1: StartServices prepends containerd + writes config.toml**

In `crates/sequencer/src/boot.rs`, the StartServices task currently does:

```rust
        let services = ctx.provider.services().to_vec();
```

Replace with:

```rust
        let rt = ctx.provider.runtime();
        if !rt.disabled {
            // Best-effort: write the minimal containerd config if absent.
            let path = std::path::Path::new(&rt.config_path);
            if !path.exists() {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(e) =
                    std::fs::write(path, machined_config::containerd_config_toml(rt))
                {
                    tracing::warn!("writing containerd config: {e}");
                }
            }
        }
        let services = machined_config::effective_services(rt, ctx.provider.services());
```

(`machined-config` is already a sequencer dependency; if `tracing` is not imported in boot.rs, use the
existing logging import style of the file.)

- [ ] **Step 2: machined registers the health controller**

`crates/machined/Cargo.toml`: add `machined-cri.workspace = true`.

In `crates/machined/src/main.rs`, add alongside the other backend builders:

```rust
fn build_cri(socket: &str) -> Arc<dyn machined_cri::CriClient> {
    Arc::new(machined_cri::GrpcCriClient::new(socket))
}
```

In `run_daemon`, after the `TimeSyncController` registration:

```rust
    runtime.register(Box::new(RuntimeHealthController::new(
        build_cri(&provider.runtime().socket),
        provider.clone(),
    )));
```

with the import `use machined_controllers::runtime::RuntimeHealthController;`.

- [ ] **Step 3: e2e — fake CRI client through the real Runtime**

Create `crates/machined/tests/runtime.rs`:

```rust
//! End-to-end: RuntimeHealthController on the real Runtime against a fake CRI
//! client publishes a ready RuntimeStatus. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{MachineConfig, MachineSection, Provider};
use machined_controllers::runtime::{RuntimeHealthController, NS};
use machined_cri::FakeCriClient;
use machined_resources::{Key, Resource, ResourceType};
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn publishes_runtime_status() {
    let cri = Arc::new(
        FakeCriClient::new()
            .with_version("containerd", "2.0.0")
            .with_ready(true),
    );
    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: Default::default(),
            install: None,
            time: Default::default(),
            runtime: Default::default(),
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(RuntimeHealthController::new(
        cri,
        Provider::new(config),
    )));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut ready = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(obj) = state.get(&Key::new(NS, ResourceType::RuntimeStatus, "containerd")) {
            if let Resource::RuntimeStatus(r) = obj.spec {
                if r.ready && r.version == "2.0.0" {
                    ready = true;
                    break;
                }
            }
        }
    }
    assert!(ready, "RuntimeStatus did not become ready");

    shutdown.cancel();
    let _ = handle.await;
}
```

- [ ] **Step 4: Full gate + commit**

Run: `cargo test -p machined --test runtime` → PASS.
Run: `make pre-commit` → fmt + clippy -D warnings + full workspace green (ignored stay ignored).

```bash
git add crates/sequencer crates/machined Cargo.lock
git commit -m "feat(machined,sequencer): inject containerd service + register CRI health"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** trimmed proto + CriClient/Grpc/Fake + UDS test + gated real test (T1) ✓; RuntimeSection + helpers (`containerd_service`/`containerd_config_toml`/`effective_services`/`validate_services` + reserved id) + RuntimeStatus + apiserver arms (T2) ✓; RuntimeHealthController (10s resync, disabled/ready/not-ready/transient) (T3) ✓; sequencer injection + config.toml write + machined registration + e2e (T4) ✓.
- **Spikes isolated:** UDS connector confined to `grpc.rs` (trait unchanged); proto field numbers wire-contract (trim messages only). The UDS fake-server tests are the acceptance gates; real-containerd test gated.
- **Name collision care:** `cri::pb::RuntimeStatus` (proto) vs `machined_resources::RuntimeStatus` — different modules, never imported into the same scope unqualified (the controller imports only the resources one; the cri crate only the pb one).
- **Type consistency:** `RuntimeSection` (config) → `containerd_service`/`effective_services` (config::runtime_svc) → sequencer StartServices; `CriClient` (cri) → `RuntimeHealthController` (controllers) → `RuntimeStatus` (resources) → apiserver mapping arms. `NetworkReady` explicitly does NOT gate (`ready()` checks `RuntimeReady` only; the UDS test pins it with NetworkReady=false).
- **Field-break follow-through:** `MachineSection.runtime` → grep ALL literals (~8 after M2c's 7 + machined/tests/time.rs) + the controller/test literals introduced in this plan already include `runtime:`.
- **Placeholder scan:** none; all steps ship complete code + exact commands.
