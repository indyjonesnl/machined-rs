# machined-rs M3a-1 — PKI + API Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M0–M2 merged to `main`. Work on branch `spec/machined-rs-m3a-mgmt-api`.

**Goal:** A node `pki` crate (rcgen CA + server/client certs) and an `apiserver` crate (a `machine.proto` tonic mTLS gRPC server exposing `Version` + a generic `ListResources` over the COSI `State`), proven by a Rust integration test that connects a mTLS client and queries seeded resources.

**Architecture:** `pki` generates a CA and CA-signed leaf certs with rcgen (pure-Rust), all PEM. `apiserver` compiles `machine.proto` with tonic-build, implements `MachineService` over a shared `runtime-core::State`, and serves it over mutual TLS (server identity + client-CA-root from the node PKI). `ListResources` maps any stored `Resource` to a `(key,value)` field list via one exhaustive match.

**Tech Stack:** `rcgen 0.13`, `tonic 0.12` + `prost 0.13` + `tonic-build 0.12` (+ `protoc-bin-vendored` so no system `protoc` is needed), `tokio`, `thiserror`. This is the project's first build-time codegen (`tonic-build`) and its first TLS — three isolated spikes (rcgen, protoc/tonic, mTLS).

---

## File Structure

```
crates/pki/
├── Cargo.toml                  # NEW
└── src/lib.rs                  # NEW: CertKey, CertRole, generate_ca/generate_cert, NodePki
crates/apiserver/
├── Cargo.toml                  # NEW
├── build.rs                    # NEW: tonic-build (vendored protoc)
├── proto/machine.proto         # NEW: MachineService
└── src/
    ├── lib.rs                  # NEW: include generated proto, serve(), re-exports
    ├── service.rs              # NEW: MachineService impl (Version, ListResources)
    └── mapping.rs              # NEW: parse_resource_type + resource_to_fields
crates/apiserver/tests/grpc.rs  # NEW: mTLS integration test
```

---

## Task 1: `pki` crate (rcgen spike)

> **SPIKE NOTE:** rcgen 0.13's API for building a CA and signing a leaf with it
> (`CertificateParams`, `KeyPair`, `params.self_signed(&key)`, `params.signed_by(...)`, the
> `Issuer`/`from_ca_cert_pem` path, `Certificate::pem()`, `KeyPair::serialize_pem()`,
> `ExtendedKeyUsagePurpose`, `KeyUsagePurpose`, `IsCa`/`BasicConstraints`, `DnType`) may differ in the
> resolved 0.13.x. The code below is best-effort. Adapt the rcgen calls to the installed version
> (the operation — a self-signed CA, then leaf certs signed by it with serverAuth/clientAuth EKU and
> SANs, emitted as PEM — is stable). Verify against the vendored source under
> `~/.cargo/registry/src`. Report every change. The certs must be real PEM that `rustls`/`tonic` can
> load (proven by the T4 mTLS handshake).

**Files:**
- Modify: `Cargo.toml` (members + deps)
- Create: `crates/pki/Cargo.toml`
- Create: `crates/pki/src/lib.rs`

- [ ] **Step 1: Add the crate + dep**

In root `Cargo.toml`, add `"crates/pki"` to `members`; add to `[workspace.dependencies]`:

```toml
# x509-parser feature is REQUIRED: CertificateParams::from_ca_cert_pem (used to
# re-import the CA before signing leaves) is gated behind it in rcgen 0.13.
rcgen = { version = "0.13", features = ["x509-parser"] }

machined-pki = { path = "crates/pki" }
```

- [ ] **Step 2: Create the manifest**

Create `crates/pki/Cargo.toml`:

```toml
[package]
name = "machined-pki"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
rcgen.workspace = true
thiserror.workspace = true
```

- [ ] **Step 3: Write the pki implementation + tests**

Create `crates/pki/src/lib.rs`:

```rust
//! Node PKI: a self-signed CA and CA-signed server/client certificates (rcgen).
//! Everything is PEM in/out so `rustls`/`tonic` can consume it directly.

use std::fs;
use std::path::Path;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};

#[derive(thiserror::Error, Debug)]
pub enum PkiError {
    #[error("rcgen: {0}")]
    Rcgen(String),
    #[error("io {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, PkiError>;

fn rc<E: std::fmt::Display>(e: E) -> PkiError {
    PkiError::Rcgen(e.to_string())
}

/// A certificate + its private key, both PEM-encoded.
#[derive(Clone, Debug)]
pub struct CertKey {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Leaf certificate role (sets the Extended Key Usage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertRole {
    Server,
    Client,
}

/// Generate a self-signed CA.
pub fn generate_ca() -> Result<CertKey> {
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(rc)?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "machined-ca");
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let key = KeyPair::generate().map_err(rc)?;
    let cert = params.self_signed(&key).map_err(rc)?;
    Ok(CertKey {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// Generate a leaf certificate signed by `ca`, with the given common name, role
/// (serverAuth/clientAuth EKU), and Subject Alternative Names.
pub fn generate_cert(ca: &CertKey, cn: &str, role: CertRole, sans: &[String]) -> Result<CertKey> {
    let ca_key = KeyPair::from_pem(&ca.key_pem).map_err(rc)?;
    let ca_params = CertificateParams::from_ca_cert_pem(&ca.cert_pem).map_err(rc)?;
    let ca_cert = ca_params.self_signed(&ca_key).map_err(rc)?;

    let mut params = CertificateParams::new(sans.to_vec()).map_err(rc)?;
    params.distinguished_name.push(DnType::CommonName, cn);
    params.extended_key_usages = vec![match role {
        CertRole::Server => ExtendedKeyUsagePurpose::ServerAuth,
        CertRole::Client => ExtendedKeyUsagePurpose::ClientAuth,
    }];
    let key = KeyPair::generate().map_err(rc)?;
    let cert = params.signed_by(&key, &ca_cert, &ca_key).map_err(rc)?;
    Ok(CertKey {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// The node's persistent PKI: a CA and a server identity, load-or-generated.
pub struct NodePki {
    ca: CertKey,
    server: CertKey,
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|source| PkiError::Io {
        path: path.to_string_lossy().to_string(),
        source,
    })
}

fn write(path: &Path, data: &str) -> Result<()> {
    fs::write(path, data).map_err(|source| PkiError::Io {
        path: path.to_string_lossy().to_string(),
        source,
    })
}

impl NodePki {
    /// Load the CA + server identity from `dir`, generating + persisting them if
    /// absent. Idempotent: a second call with the same dir loads the same CA.
    pub fn load_or_generate(dir: &Path, server_cn: &str, server_sans: &[String]) -> Result<Self> {
        fs::create_dir_all(dir).map_err(|source| PkiError::Io {
            path: dir.to_string_lossy().to_string(),
            source,
        })?;
        let cap = dir.join("ca.pem");
        let cak = dir.join("ca.key");
        let sp = dir.join("server.pem");
        let sk = dir.join("server.key");

        if cap.exists() && cak.exists() && sp.exists() && sk.exists() {
            return Ok(Self {
                ca: CertKey {
                    cert_pem: read(&cap)?,
                    key_pem: read(&cak)?,
                },
                server: CertKey {
                    cert_pem: read(&sp)?,
                    key_pem: read(&sk)?,
                },
            });
        }

        let ca = generate_ca()?;
        let server = generate_cert(&ca, server_cn, CertRole::Server, server_sans)?;
        write(&cap, &ca.cert_pem)?;
        write(&cak, &ca.key_pem)?;
        write(&sp, &server.cert_pem)?;
        write(&sk, &server.key_pem)?;
        Ok(Self { ca, server })
    }

    pub fn server_identity(&self) -> (String, String) {
        (self.server.cert_pem.clone(), self.server.key_pem.clone())
    }

    pub fn ca_pem(&self) -> String {
        self.ca.cert_pem.clone()
    }

    /// Issue a fresh client certificate signed by the node CA.
    pub fn issue_client(&self, cn: &str) -> Result<CertKey> {
        generate_cert(&self.ca, cn, CertRole::Client, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_pem(s: &str, label: &str) -> bool {
        s.contains(&format!("-----BEGIN {label}-----"))
            && s.contains(&format!("-----END {label}-----"))
    }

    #[test]
    fn generates_ca_and_leaf_pem() {
        let ca = generate_ca().unwrap();
        assert!(is_pem(&ca.cert_pem, "CERTIFICATE"));
        assert!(is_pem(&ca.key_pem, "PRIVATE KEY"));

        let server = generate_cert(&ca, "node", CertRole::Server, &["127.0.0.1".into()]).unwrap();
        assert!(is_pem(&server.cert_pem, "CERTIFICATE"));
        let client = generate_cert(&ca, "admin", CertRole::Client, &[]).unwrap();
        assert!(is_pem(&client.cert_pem, "CERTIFICATE"));
    }

    #[test]
    fn node_pki_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("mnd-pki-{}", std::process::id()));
        let p1 = NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        let p2 = NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        // Second call loads the same CA, not a fresh one.
        assert_eq!(p1.ca_pem(), p2.ca_pem());
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

> **Review follow-up (applied):** private key files (`ca.key`, `server.key`) are written `0600` on
> Unix (a `write_key` helper that `set_permissions(0o600)` after `write`), so the CA/server private
> keys are never world-readable. A `private_key_files_are_owner_only` test asserts the mode. (Full
> permission/STATE-volume hardening of the whole PKI dir remains a later milestone.)

- [ ] **Step 4: Build (spike gate) + test + commit**

Run: `cargo build -p machined-pki`
Expected: PASS. If rcgen 0.13 API differs, adapt per the SPIKE NOTE until it builds; record changes.

Run: `cargo test -p machined-pki`
Expected: PASS — `generates_ca_and_leaf_pem`, `node_pki_is_idempotent`.

Run: `cargo clippy -p machined-pki --all-targets -- -D warnings`
Expected: clean.

```bash
cargo fmt --all
git add Cargo.toml Cargo.lock crates/pki
git commit -m "feat(pki): rcgen node CA + server/client certs + NodePki"
```

---

## Task 2: `apiserver` scaffold — proto + Version (protoc/tonic spike)

> **SPIKE NOTE:** `tonic-build` needs `protoc`. This plan vendors it via `protoc-bin-vendored` so no
> system `protoc` is required — `build.rs` sets `PROTOC` to the vendored binary. If the resolved
> `tonic`/`prost`/`tonic-build` 0.12/0.13 codegen API differs (`tonic::include_proto!`,
> `MachineServiceServer`, the generated request/response types, `tonic::transport::Server`), adapt and
> report. The plaintext Version integration test is the acceptance gate for the codegen + transport.

**Files:**
- Modify: `Cargo.toml` (members + deps)
- Create: `crates/apiserver/Cargo.toml`
- Create: `crates/apiserver/build.rs`
- Create: `crates/apiserver/proto/machine.proto`
- Create: `crates/apiserver/src/lib.rs`
- Create: `crates/apiserver/src/service.rs`
- Create: `crates/apiserver/tests/grpc.rs`

- [ ] **Step 1: Add the workspace deps**

In root `Cargo.toml`, add `"crates/apiserver"` to `members`; add to `[workspace.dependencies]`:

```toml
tonic = "0.12"
prost = "0.13"
tonic-build = "0.12"
protoc-bin-vendored = "3"

machined-apiserver = { path = "crates/apiserver" }
```

- [ ] **Step 2: Create the manifest + build.rs + proto**

Create `crates/apiserver/Cargo.toml`:

```toml
[package]
name = "machined-apiserver"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
machined-resources.workspace = true
machined-runtime-core.workspace = true
machined-pki.workspace = true
tonic.workspace = true
prost.workspace = true
tokio.workspace = true
tracing.workspace = true

[build-dependencies]
tonic-build.workspace = true
protoc-bin-vendored.workspace = true

[dev-dependencies]
tokio = { workspace = true }
```

Create `crates/apiserver/build.rs`:

```rust
fn main() {
    // Vendor protoc so no system install is required.
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);
    tonic_build::compile_protos("proto/machine.proto").expect("compile machine.proto");
}
```

Create `crates/apiserver/proto/machine.proto`:

```proto
syntax = "proto3";
package machine;

service MachineService {
  rpc Version(Empty) returns (VersionResponse);
  rpc ListResources(ListResourcesRequest) returns (ListResourcesResponse);
}

message Empty {}
message VersionResponse { string version = 1; }

message ListResourcesRequest {
  string namespace = 1;
  string type = 2;
}
message KeyValue {
  string key = 1;
  string value = 2;
}
message ResourceEntry {
  string id = 1;
  repeated KeyValue fields = 2;
}
message ListResourcesResponse { repeated ResourceEntry entries = 1; }
```

- [ ] **Step 3: Create the service impl (Version only) + crate root**

Create `crates/apiserver/src/service.rs`:

```rust
//! `MachineService` gRPC implementation over the COSI store.

use machined_runtime_core::State;
use tonic::{Request, Response, Status};

use crate::pb::machine_service_server::MachineService;
use crate::pb::{Empty, ListResourcesRequest, ListResourcesResponse, VersionResponse};

/// gRPC service backed by the resource store.
pub struct Machine {
    state: State,
    version: String,
}

impl Machine {
    pub fn new(state: State, version: impl Into<String>) -> Self {
        Self {
            state,
            version: version.into(),
        }
    }
}

#[tonic::async_trait]
impl MachineService for Machine {
    async fn version(&self, _req: Request<Empty>) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: self.version.clone(),
        }))
    }

    async fn list_resources(
        &self,
        req: Request<ListResourcesRequest>,
    ) -> Result<Response<ListResourcesResponse>, Status> {
        // Filled in Task 3; placeholder so the service compiles in Task 2.
        let _ = (&self.state, req);
        Ok(Response::new(ListResourcesResponse {
            entries: Vec::new(),
        }))
    }
}
```

Create `crates/apiserver/src/lib.rs`:

```rust
//! machined management gRPC API server.

pub mod mapping;
pub mod service;

/// Generated protobuf types.
pub mod pb {
    tonic::include_proto!("machine");
}

use std::net::SocketAddr;

use machined_pki::NodePki;
use machined_runtime_core::State;
use tonic::transport::Server;

pub use service::Machine;

/// Serve the management API over mutual TLS until the process exits.
pub async fn serve(
    addr: SocketAddr,
    state: State,
    version: impl Into<String>,
    pki: &NodePki,
) -> Result<(), tonic::transport::Error> {
    let svc = pb::machine_service_server::MachineServiceServer::new(Machine::new(state, version));
    let tls = server_tls(pki);
    Server::builder()
        .tls_config(tls)?
        .add_service(svc)
        .serve(addr)
        .await
}

/// Build the mutual-TLS config (server identity + required client CA). Filled in
/// Task 4; for Task 2 it is plaintext so the codegen/transport can be validated.
fn server_tls(_pki: &NodePki) -> tonic::transport::ServerTlsConfig {
    tonic::transport::ServerTlsConfig::new()
}
```

> NOTE for Task 2: `Server::builder().tls_config(ServerTlsConfig::new())` with an empty config will
> fail to serve TLS. For Task 2 only, to validate the codegen + transport in **plaintext**, the
> integration test (Step 4) builds the server WITHOUT `.tls_config` (see the test). Task 4 replaces
> `server_tls` with a real mTLS config and switches `serve` + the test to TLS. Keep `serve` as written
> (it is the final TLS form); Task 2's test exercises a plaintext server it builds inline.

- [ ] **Step 4: Create a placeholder mapping module + the plaintext integration test**

Create `crates/apiserver/src/mapping.rs` (filled in Task 3):

```rust
// parse_resource_type + resource_to_fields land in Task 3.
```

Create `crates/apiserver/tests/grpc.rs`:

```rust
//! gRPC integration tests. Task 2 = plaintext Version; Task 4 adds mTLS.

use std::time::Duration;

use machined_apiserver::pb::machine_service_client::MachineServiceClient;
use machined_apiserver::pb::Empty;
use machined_apiserver::Machine;
use machined_runtime_core::State;
use tonic::transport::Server;

#[tokio::test]
async fn version_over_plaintext() {
    let state = State::new();
    let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
        Machine::new(state, "9.9.9"),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client = MachineServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let resp = client.version(Empty {}).await.unwrap().into_inner();
    assert_eq!(resp.version, "9.9.9");
}
```

> The test uses `tokio_stream::wrappers::TcpListenerStream` to serve on an OS-assigned port. Add
> `tokio-stream` to `apiserver` `[dev-dependencies]` (`tokio-stream = "0.1"` in workspace deps, then
> `tokio-stream = { workspace = true }`). If `tokio-stream` is not yet a workspace dep, add it.

- [ ] **Step 5: Build (spike gate) + test + commit**

Run: `cargo build -p machined-apiserver`
Expected: PASS — `tonic-build` compiles the proto via the vendored protoc. If codegen/transport API differs, adapt per the SPIKE NOTE; record changes.

Run: `cargo test -p machined-apiserver --test grpc`
Expected: PASS — `version_over_plaintext`.

Run: `cargo clippy -p machined-apiserver --all-targets -- -D warnings`
Expected: clean.

```bash
cargo fmt --all
git add Cargo.toml Cargo.lock crates/apiserver
git commit -m "feat(apiserver): machine.proto + tonic server scaffold + Version"
```

---

## Task 3: `ListResources` — store query mapping

**Files:**
- Modify: `crates/apiserver/src/mapping.rs`
- Modify: `crates/apiserver/src/service.rs`
- Modify: `crates/apiserver/tests/grpc.rs`

- [ ] **Step 1: Write the mapping functions + tests**

Replace `crates/apiserver/src/mapping.rs` with:

```rust
//! Map the closed `Resource` enum to gRPC field lists, and parse a type name.

use machined_resources::{Resource, ResourceType};

/// Parse a `ResourceType` from its `Display` name (reverse of `Display`).
pub fn parse_resource_type(s: &str) -> Option<ResourceType> {
    Some(match s {
        "MachineConfig" => ResourceType::MachineConfig,
        "ServiceStatus" => ResourceType::ServiceStatus,
        "LinkSpec" => ResourceType::LinkSpec,
        "AddressSpec" => ResourceType::AddressSpec,
        "RouteSpec" => ResourceType::RouteSpec,
        "HostnameSpec" => ResourceType::HostnameSpec,
        "ResolverSpec" => ResourceType::ResolverSpec,
        "LinkStatus" => ResourceType::LinkStatus,
        "AddressStatus" => ResourceType::AddressStatus,
        "RouteStatus" => ResourceType::RouteStatus,
        "DiskStatus" => ResourceType::DiskStatus,
        "DiscoveredVolume" => ResourceType::DiscoveredVolume,
        "VolumeStatus" => ResourceType::VolumeStatus,
        "MountStatus" => ResourceType::MountStatus,
        "TimeStatus" => ResourceType::TimeStatus,
        _ => return None,
    })
}

fn kv(k: &str, v: impl ToString) -> (String, String) {
    (k.to_string(), v.to_string())
}

fn opt(v: &Option<impl ToString>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_default()
}

/// Render a resource's spec as a list of `(key, value)` fields for the API.
/// Exhaustive over the closed `Resource` enum — a new variant is a compile error.
pub fn resource_to_fields(spec: &Resource) -> Vec<(String, String)> {
    match spec {
        Resource::MachineConfig(c) => vec![kv("bytes", c.raw_yaml.len())],
        Resource::ServiceStatus(s) => vec![
            kv("service_id", &s.service_id),
            kv("state", format!("{:?}", s.state)),
            kv("healthy", s.healthy),
            kv("message", &s.last_message),
        ],
        Resource::LinkSpec(l) => vec![kv("name", &l.name), kv("up", l.up), kv("mtu", opt(&l.mtu))],
        Resource::AddressSpec(a) => vec![kv("link", &a.link), kv("address", a.address)],
        Resource::RouteSpec(r) => vec![
            kv("link", &r.link),
            kv("destination", opt(&r.destination)),
            kv("gateway", opt(&r.gateway)),
            kv("metric", r.metric),
        ],
        Resource::HostnameSpec(h) => vec![kv("hostname", &h.hostname)],
        Resource::ResolverSpec(r) => vec![
            kv("nameservers", r.nameservers.len()),
            kv("search", r.search.join(",")),
        ],
        Resource::LinkStatus(l) => vec![
            kv("name", &l.name),
            kv("up", l.up),
            kv("mtu", l.mtu),
            kv("mac", &l.mac),
        ],
        Resource::AddressStatus(a) => vec![kv("link", &a.link), kv("address", a.address)],
        Resource::RouteStatus(r) => vec![
            kv("link", &r.link),
            kv("destination", opt(&r.destination)),
            kv("gateway", opt(&r.gateway)),
        ],
        Resource::DiskStatus(d) => vec![
            kv("name", &d.name),
            kv("path", &d.path),
            kv("size_bytes", d.size_bytes),
            kv("model", &d.model),
            kv("rotational", d.rotational),
            kv("read_only", d.read_only),
        ],
        Resource::DiscoveredVolume(v) => vec![
            kv("device", &v.device),
            kv("disk", &v.disk),
            kv("partition_label", &v.partition_label),
            kv("fs_type", opt(&v.fs_type)),
            kv("size_bytes", v.size_bytes),
        ],
        Resource::VolumeStatus(v) => vec![
            kv("name", &v.name),
            kv("device", &v.device),
            kv("fs", &v.fs),
            kv("label", &v.label),
            kv("phase", format!("{:?}", v.phase)),
        ],
        Resource::MountStatus(m) => vec![
            kv("volume", &m.volume),
            kv("source", &m.source),
            kv("target", &m.target),
            kv("fstype", &m.fstype),
            kv("mounted", m.mounted),
        ],
        Resource::TimeStatus(t) => vec![
            kv("synced", t.synced),
            kv("server", &t.server),
            kv("offset_ns", t.offset_ns),
            kv("sync_count", t.sync_count),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{Resource, ServiceState, ServiceStatusSpec, TimeStatus};

    #[test]
    fn type_name_round_trips_display() {
        for t in [
            ResourceType::ServiceStatus,
            ResourceType::DiskStatus,
            ResourceType::TimeStatus,
            ResourceType::MountStatus,
        ] {
            assert_eq!(parse_resource_type(&t.to_string()), Some(t));
        }
        assert_eq!(parse_resource_type("Nonsense"), None);
    }

    #[test]
    fn maps_fields() {
        let svc = Resource::ServiceStatus(ServiceStatusSpec {
            service_id: "etcd".into(),
            state: ServiceState::Running,
            healthy: true,
            last_message: "ok".into(),
        });
        let f = resource_to_fields(&svc);
        assert!(f.contains(&("service_id".to_string(), "etcd".to_string())));
        assert!(f.contains(&("healthy".to_string(), "true".to_string())));

        let t = Resource::TimeStatus(TimeStatus {
            synced: true,
            server: "a".into(),
            offset_ns: -5,
            sync_count: 2,
        });
        assert!(resource_to_fields(&t).contains(&("offset_ns".to_string(), "-5".to_string())));
    }
}
```

> `AddrCidr`, `ServiceState`, `VolumePhase`, `IpAddr`, etc. must render via `to_string()`/`{:?}`:
> `AddrCidr` and `IpAddr` implement `Display`; `ServiceState`/`VolumePhase` use `{:?}` (Debug) here.
> Confirm each field expression compiles against the actual resource struct fields (see
> `crates/resources/src/{network,block,resource,time}.rs`); adjust a field name if it differs.

- [ ] **Step 2: Implement the `list_resources` handler**

In `crates/apiserver/src/service.rs`, replace the placeholder `list_resources` with:

```rust
    async fn list_resources(
        &self,
        req: Request<ListResourcesRequest>,
    ) -> Result<Response<ListResourcesResponse>, Status> {
        let r = req.into_inner();
        let typ = crate::mapping::parse_resource_type(&r.r#type)
            .ok_or_else(|| Status::invalid_argument(format!("unknown resource type: {}", r.r#type)))?;
        let entries = self
            .state
            .list(&r.namespace, typ)
            .into_iter()
            .map(|obj| {
                let fields = crate::mapping::resource_to_fields(&obj.spec)
                    .into_iter()
                    .map(|(key, value)| crate::pb::KeyValue { key, value })
                    .collect();
                crate::pb::ResourceEntry {
                    id: obj.metadata.id,
                    fields,
                }
            })
            .collect();
        Ok(Response::new(ListResourcesResponse { entries }))
    }
```

(The `r#type` raw identifier is the generated field for the proto `type` field.)

- [ ] **Step 3: Extend the integration test to query a seeded resource**

Append to `crates/apiserver/tests/grpc.rs`:

```rust
#[tokio::test]
async fn lists_seeded_resources_over_plaintext() {
    use machined_apiserver::pb::ListResourcesRequest;
    use machined_resources::{Resource, ResourceObject, ServiceState, ServiceStatusSpec};

    let state = State::new();
    state
        .create(ResourceObject::new(
            "runtime",
            "etcd",
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: "ok".into(),
            }),
        ))
        .unwrap();

    let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
        Machine::new(state, "9.9.9"),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client = MachineServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let resp = client
        .list_resources(ListResourcesRequest {
            namespace: "runtime".into(),
            r#type: "ServiceStatus".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.entries.len(), 1);
    assert_eq!(resp.entries[0].id, "etcd");
    assert!(resp.entries[0]
        .fields
        .iter()
        .any(|f| f.key == "service_id" && f.value == "etcd"));
}
```

- [ ] **Step 4: Test + clippy + commit**

Run: `cargo test -p machined-apiserver` → mapping unit tests + both integration tests pass.
Run: `cargo clippy -p machined-apiserver --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/apiserver
git commit -m "feat(apiserver): ListResources over the COSI store"
```

---

## Task 4: mutual TLS (mTLS spike)

> **SPIKE NOTE:** tonic 0.12 transport TLS uses its own rustls. `ServerTlsConfig::new().identity(
> Identity::from_pem(cert, key)).client_ca_root(Certificate::from_pem(ca))` and the client
> `ClientTlsConfig::new().ca_certificate(...).identity(...)` + `Endpoint::tls_config(...)` are
> best-effort; adapt names if 0.12 differs (e.g. `Identity`, `Certificate` import paths under
> `tonic::transport`). The server cert SAN must include `127.0.0.1` (the loopback the client dials).
> Report changes. The two integration assertions (authorized client succeeds, unauthenticated client
> rejected) are the acceptance proof.

**Files:**
- Modify: `crates/apiserver/src/lib.rs`
- Modify: `crates/apiserver/tests/grpc.rs`

- [ ] **Step 1: Implement the real mTLS server config**

In `crates/apiserver/src/lib.rs`, replace `server_tls` with:

```rust
fn server_tls(pki: &NodePki) -> tonic::transport::ServerTlsConfig {
    use tonic::transport::{Certificate, Identity, ServerTlsConfig};
    let (cert, key) = pki.server_identity();
    ServerTlsConfig::new()
        .identity(Identity::from_pem(cert, key))
        .client_ca_root(Certificate::from_pem(pki.ca_pem()))
}
```

- [ ] **Step 2: Add the mTLS integration test**

Append to `crates/apiserver/tests/grpc.rs`:

```rust
#[tokio::test]
async fn mtls_requires_a_valid_client_cert() {
    use machined_apiserver::pb::machine_service_client::MachineServiceClient;
    use machined_apiserver::pb::Empty;
    use machined_pki::NodePki;
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

    let dir = std::env::temp_dir().join(format!("mnd-api-tls-{}", std::process::id()));
    let pki = NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
    let ca = pki.ca_pem();
    let client_id = pki.issue_client("admin").unwrap();

    let state = State::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let pki_moved = pki;
    tokio::spawn(async move {
        let tls = {
            use tonic::transport::{Identity as Id, ServerTlsConfig};
            let (c, k) = pki_moved.server_identity();
            ServerTlsConfig::new()
                .identity(Id::from_pem(c, k))
                .client_ca_root(Certificate::from_pem(pki_moved.ca_pem()))
        };
        let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
            machined_apiserver::Machine::new(state, "9.9.9"),
        );
        Server::builder()
            .tls_config(tls)
            .unwrap()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Authorized client (CA-signed cert) succeeds.
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&ca))
        .identity(Identity::from_pem(&client_id.cert_pem, &client_id.key_pem))
        .domain_name("127.0.0.1");
    let channel = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = MachineServiceClient::new(channel);
    let resp = client.version(Empty {}).await.unwrap().into_inner();
    assert_eq!(resp.version, "9.9.9");

    // Unauthenticated client (no client identity) is rejected at the handshake.
    let tls_no_id = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&ca))
        .domain_name("127.0.0.1");
    let bad = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls_no_id)
        .unwrap()
        .connect()
        .await;
    assert!(bad.is_err() || {
        let mut c = MachineServiceClient::new(bad.unwrap());
        c.version(Empty {}).await.is_err()
    });

    std::fs::remove_dir_all(&dir).ok();
}
```

> The plaintext tests from Tasks 2–3 stay (they exercise the handlers without TLS). This test proves
> mTLS: a CA-signed client works; a client with no identity fails (at connect or first RPC).

- [ ] **Step 3: Full gate + commit**

Run: `cargo test -p machined-apiserver` → plaintext (2) + mapping unit + `mtls_requires_a_valid_client_cert` pass.
Run: `cargo build --workspace` → PASS.
Run: `cargo clippy --all-targets --all-features -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/apiserver
git commit -m "feat(apiserver): mutual TLS (server identity + client CA)"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage (M3a spec, M3a-1 portion):** `pki` crate (rcgen CA + server/client certs + NodePki load-or-generate) (Task 1) ✓; `apiserver` proto + tonic server + `Version` (Task 2) ✓; `ListResources` generic store query via exhaustive `resource_to_fields` + `parse_resource_type` (Task 3) ✓; mTLS server (identity + client CA) with an authorized-succeeds / unauthenticated-rejected integration test (Task 4) ✓.
- **Three isolated spikes:** rcgen 0.13 (Task 1), protoc/tonic-build codegen (Task 2, vendored protoc so no system dep), tonic mTLS (Task 4). Each has a SPIKE NOTE; the trait/proto/handler logic is otherwise deterministic.
- **Deliberate M3a-1 limits (per spec):** read-only RPCs only (Version + ListResources); no actions (M3c), no streaming/typed-detail RPCs (M3b), no `machinectl` (M3a-2), no machined wiring (M3a-2). Server runs are validated by integration tests here.
- **Type consistency:** `NodePki`/`CertKey`/`CertRole` (pki) ↔ `apiserver::serve`/`Machine`/`server_tls`; `resource_to_fields` covers every `Resource` variant from `resources`; the generated `pb::*` types are the contract the M3a-2 `machinectl` client will reuse.
- **Field-mapping caveat:** `resource_to_fields` references each resource struct's fields; if a field name differs from this plan, fix it against `crates/resources/src/*` — the exhaustive match guarantees nothing is missed.
- **Placeholder scan:** none; the rcgen/tonic/mTLS code is real best-effort with explicit spike protocols, and Task 2's `list_resources`/`mapping.rs` placeholders are explicitly replaced in Task 3.
