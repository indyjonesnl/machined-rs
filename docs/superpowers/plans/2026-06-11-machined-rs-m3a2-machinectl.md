# machined-rs M3a-2 — machinectl CLI + wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M3a-1 (pki + apiserver) merged to `main`. Work on branch `spec/machined-rs-m3a2-machinectl`.

**Goal:** A `machinectl` CLI that connects to the node's mTLS gRPC API and runs `version` + `get <type>`, plus the `machined` wiring that loads/generates the node PKI, writes a client bundle, and spawns the API server — closing the M3a slice (a real CLI talks to the node over mutual TLS).

**Architecture:** `machinectl` is a `clap` binary that builds a tonic mTLS `Channel` from a bundle directory (`ca.pem` + `client.pem` + `client.key`) and calls the generated `MachineServiceClient`. `machined::run_daemon` calls `NodePki::load_or_generate`, writes a `machinectl` client bundle, and `tokio::spawn`s `apiserver::serve` on `127.0.0.1:50000` with a clone of the COSI `State`. An end-to-end test runs the built `machinectl` binary against a real mTLS server.

**Tech Stack:** `clap 4` (derive), `tonic 0.12` (tls), the M3a-1 `apiserver` (`pb` client) + `pki` crates, `tokio`.

---

## File Structure

```
crates/machinectl/
├── Cargo.toml              # NEW
├── src/main.rs             # NEW: clap CLI + mTLS connect + version/get
└── tests/e2e.rs            # NEW: run the built binary against a real mTLS server
crates/machined/
├── Cargo.toml              # MODIFY: + machined-apiserver, machined-pki deps
└── src/main.rs             # MODIFY: load PKI, write bundle, spawn apiserver
```

---

## Task 1: `machinectl` crate

**Files:**
- Modify: `Cargo.toml` (members + clap dep)
- Create: `crates/machinectl/Cargo.toml`
- Create: `crates/machinectl/src/main.rs`

- [ ] **Step 1: Add the crate + clap dep**

In root `Cargo.toml`, add `"crates/machinectl"` to `members`; add to `[workspace.dependencies]`:

```toml
clap = { version = "4", features = ["derive"] }
```

- [ ] **Step 2: Create the manifest**

Create `crates/machinectl/Cargo.toml`:

```toml
[package]
name = "machinectl"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "machinectl"
path = "src/main.rs"

[dependencies]
machined-apiserver.workspace = true
clap.workspace = true
tonic.workspace = true
tokio.workspace = true
anyhow.workspace = true

[dev-dependencies]
machined-pki.workspace = true
machined-runtime-core.workspace = true
machined-resources.workspace = true
tokio-stream.workspace = true
```

> If `anyhow` is not yet a `[workspace.dependencies]` entry, add `anyhow = "1"` (it is already used by
> the `machined` crate, so it should be present — reuse it).

- [ ] **Step 3: Write the CLI**

Create `crates/machinectl/src/main.rs`:

```rust
//! machinectl — the machined management CLI (mutual-TLS gRPC client).

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use machined_apiserver::pb::machine_service_client::MachineServiceClient;
use machined_apiserver::pb::{Empty, ListResourcesRequest};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// machined management CLI.
#[derive(Parser)]
#[command(name = "machinectl", version)]
struct Cli {
    /// Directory holding ca.pem, client.pem, client.key.
    #[arg(long, default_value = "/system/state/pki/machinectl")]
    bundle: PathBuf,
    /// API endpoint.
    #[arg(long, default_value = "https://127.0.0.1:50000")]
    endpoint: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the machined version.
    Version,
    /// List resources of a type (e.g. ServiceStatus, DiskStatus, TimeStatus).
    Get {
        /// Resource type name.
        resource_type: String,
        /// Resource namespace.
        #[arg(long, default_value = "runtime")]
        namespace: String,
    },
}

fn read(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))
}

/// Build a mutual-TLS client from the bundle directory.
async fn connect(bundle: &Path, endpoint: &str) -> anyhow::Result<MachineServiceClient<Channel>> {
    let ca = read(&bundle.join("ca.pem"))?;
    let cert = read(&bundle.join("client.pem"))?;
    let key = read(&bundle.join("client.key"))?;
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca))
        .identity(Identity::from_pem(cert, key))
        .domain_name("127.0.0.1");
    let channel = Endpoint::from_shared(endpoint.to_string())?
        .tls_config(tls)?
        .connect()
        .await?;
    Ok(MachineServiceClient::new(channel))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut client = connect(&cli.bundle, &cli.endpoint).await?;
    match cli.command {
        Command::Version => {
            let v = client.version(Empty {}).await?.into_inner();
            println!("{}", v.version);
        }
        Command::Get {
            resource_type,
            namespace,
        } => {
            let resp = client
                .list_resources(ListResourcesRequest {
                    namespace,
                    r#type: resource_type,
                })
                .await?
                .into_inner();
            for e in resp.entries {
                let fields = e
                    .fields
                    .iter()
                    .map(|kv| format!("{}={}", kv.key, kv.value))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("{}\t{}", e.id, fields);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_get_with_namespace() {
        let cli = Cli::try_parse_from([
            "machinectl",
            "--bundle",
            "/tmp/b",
            "get",
            "ServiceStatus",
            "--namespace",
            "block",
        ])
        .unwrap();
        match cli.command {
            Command::Get {
                resource_type,
                namespace,
            } => {
                assert_eq!(resource_type, "ServiceStatus");
                assert_eq!(namespace, "block");
            }
            _ => panic!("expected Get"),
        }
        assert_eq!(cli.bundle, PathBuf::from("/tmp/b"));
    }

    #[test]
    fn version_defaults() {
        let cli = Cli::try_parse_from(["machinectl", "version"]).unwrap();
        assert!(matches!(cli.command, Command::Version));
        assert_eq!(cli.endpoint, "https://127.0.0.1:50000");
    }
}
```

- [ ] **Step 4: Build + test + commit**

Run: `cargo build -p machinectl` → PASS.
Run: `cargo test -p machinectl --bin machinectl` → `parses_get_with_namespace`, `version_defaults` pass.
Run: `cargo clippy -p machinectl --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add Cargo.toml Cargo.lock crates/machinectl
git commit -m "feat(machinectl): mTLS gRPC CLI (version + get)"
```

---

## Task 2: `machined` wiring — PKI + spawn the API server

**Files:**
- Modify: `crates/machined/Cargo.toml`
- Modify: `crates/machined/src/main.rs`

- [ ] **Step 1: Add the deps**

In `crates/machined/Cargo.toml` `[dependencies]`, add:

```toml
machined-apiserver.workspace = true
machined-pki.workspace = true
```

- [ ] **Step 2: Add imports + a bundle-writer helper**

In `crates/machined/src/main.rs`, add near the other `use` lines:

```rust
use std::net::SocketAddr;
use std::path::Path;

use machined_pki::NodePki;
```

Add this helper function (top-level, near `build_platform`/`build_time_sync`):

```rust
/// Write a machinectl client bundle (ca + a fresh client cert) into `dir`.
fn write_client_bundle(dir: &Path, pki: &NodePki) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let client = pki.issue_client("machinectl")?;
    std::fs::write(dir.join("ca.pem"), pki.ca_pem())?;
    std::fs::write(dir.join("client.pem"), &client.cert_pem)?;
    std::fs::write(dir.join("client.key"), &client.key_pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("client.key"), std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
```

- [ ] **Step 3: Spawn the API server before the SequencerCtx moves `state`**

In `run_daemon`, immediately AFTER the runtime spawn (the `let rt_handle = tokio::spawn(...)` block,
currently ending around line 172) and BEFORE the `let ctx = SequencerCtx { state, ... }` line (which
moves `state`), insert:

```rust
    // Management API (M3a): node PKI + mTLS gRPC server, sharing the COSI store.
    let pki_dir = std::path::PathBuf::from("/system/state/pki");
    match NodePki::load_or_generate(&pki_dir, "node", &["127.0.0.1".into(), "localhost".into()]) {
        Ok(pki) => {
            if let Err(e) = write_client_bundle(&pki_dir.join("machinectl"), &pki) {
                error!("writing machinectl bundle: {e}");
            }
            let api_addr: SocketAddr = "127.0.0.1:50000".parse().unwrap();
            let api_state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = machined_apiserver::serve(
                    api_addr,
                    api_state,
                    env!("CARGO_PKG_VERSION"),
                    &pki,
                )
                .await
                {
                    error!("apiserver exited: {e}");
                }
            });
            info!("management API listening on {api_addr}");
        }
        Err(e) => error!("PKI init failed; management API disabled: {e}"),
    }
```

> `state` is `runtime.state()` (a cheap `Clone` handle). The `state.clone()` here is consumed by the
> server task; the original `state` still moves into `SequencerCtx` on the next line. A PKI or serve
> failure logs and the daemon continues (the node stays up without the API), per design §4.

- [ ] **Step 4: Build + smoke + commit**

Run: `cargo build --workspace` → PASS.
Run: `cargo run -p machined -- version` → `machined 0.1.0` (the `version` subcommand path does not
start the daemon, so it does not bind/serve).
Run: `cargo clippy --all-targets --all-features -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/machined Cargo.lock
git commit -m "feat(machined): load PKI + spawn mTLS API server + machinectl bundle"
```

---

## Task 3: end-to-end test (built binary ↔ real mTLS server)

**Files:**
- Create: `crates/machinectl/tests/e2e.rs`

- [ ] **Step 1: Write the e2e test**

Create `crates/machinectl/tests/e2e.rs`:

```rust
//! End-to-end: run the built `machinectl` binary against a real mTLS apiserver.
//! Root-free (loopback TLS). Uses the cargo-provided binary path.

use std::time::Duration;

use machined_pki::NodePki;
use machined_resources::{Resource, ResourceObject, ServiceState, ServiceStatusSpec};
use machined_runtime_core::State;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

#[tokio::test]
async fn machinectl_queries_a_real_server() {
    // PKI + a machinectl bundle on disk.
    let root = std::env::temp_dir().join(format!("mnd-mctl-{}", std::process::id()));
    let pki_dir = root.join("pki");
    let pki = NodePki::load_or_generate(&pki_dir, "node", &["127.0.0.1".into()]).unwrap();
    let bundle = root.join("bundle");
    std::fs::create_dir_all(&bundle).unwrap();
    let client = pki.issue_client("test").unwrap();
    std::fs::write(bundle.join("ca.pem"), pki.ca_pem()).unwrap();
    std::fs::write(bundle.join("client.pem"), &client.cert_pem).unwrap();
    std::fs::write(bundle.join("client.key"), &client.key_pem).unwrap();

    // A store with one seeded ServiceStatus.
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

    // mTLS server on an ephemeral loopback port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let (scert, skey) = pki.server_identity();
    let ca_pem = pki.ca_pem();
    tokio::spawn(async move {
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(scert, skey))
            .client_ca_root(Certificate::from_pem(ca_pem));
        let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
            machined_apiserver::Machine::new(state, "1.2.3"),
        );
        Server::builder()
            .tls_config(tls)
            .unwrap()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(120)).await;

    let endpoint = format!("https://{addr}");
    let bin = env!("CARGO_BIN_EXE_machinectl");

    // `version`
    let out = tokio::process::Command::new(bin)
        .args([
            "--bundle",
            bundle.to_str().unwrap(),
            "--endpoint",
            &endpoint,
            "version",
        ])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "version failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1.2.3"), "version stdout: {stdout}");

    // `get ServiceStatus`
    let out2 = tokio::process::Command::new(bin)
        .args([
            "--bundle",
            bundle.to_str().unwrap(),
            "--endpoint",
            &endpoint,
            "get",
            "ServiceStatus",
            "--namespace",
            "runtime",
        ])
        .output()
        .await
        .unwrap();
    assert!(out2.status.success(), "get failed: {:?}", out2);
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("etcd"), "get stdout: {stdout2}");

    std::fs::remove_dir_all(&root).ok();
}
```

> `CARGO_BIN_EXE_machinectl` is set by cargo for integration tests of a binary crate, so the test runs
> the actual built CLI. `tokio::process::Command` (async) avoids blocking the runtime that hosts the
> server task. The server uses `serve_with_incoming` on a `127.0.0.1:0` port so the test is
> non-flaky (no fixed-port collision).

- [ ] **Step 2: Run + full gate + commit**

Run: `cargo test -p machinectl` → unit (2) + `machinectl_queries_a_real_server` pass.
Run: `make pre-commit` → fmt + clippy -D warnings + full workspace test green (ignored tests stay ignored).

```bash
git add crates/machinectl
git commit -m "test(machinectl): e2e against a real mTLS server"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage (M3a spec, M3a-2 portion):** `machinectl` CLI (`version` + `get <type> [--namespace]`, `--bundle`/`--endpoint`, mTLS client from ca/client.pem/client.key) §3.4 (Task 1) ✓; `machined` wiring (load_or_generate PKI, write client bundle, spawn `apiserver::serve` on 127.0.0.1:50000 with a `State` clone) §3.5 (Task 2) ✓; e2e — spawn the server, run the `machinectl` binary, assert `version` + `get ServiceStatus` output §5 (Task 3) ✓.
- **Reuses the M3a-1 contract:** `machinectl` depends only on `machined_apiserver::pb` (the generated client) + `tonic` — the same proto types the server exposes. No new proto, no new RPCs (those are M3b/M3c).
- **Deliberate M3a-2 limits:** read-only `version`/`get`; fixed `127.0.0.1:50000` + `domain_name("127.0.0.1")` (single-node loopback; configurable endpoints are later); the client bundle + PKI live under `/system/state/pki` (real STATE-volume permission hardening is deferred). `client.key` in the bundle is written `0600`.
- **Wiring safety:** `state.clone()` for the API task before `state` moves into `SequencerCtx`; a PKI/serve failure logs and the node continues (non-fatal), per design §4.
- **Type consistency:** `connect()` returns `MachineServiceClient<Channel>`; `Get.resource_type`/`namespace` feed `ListResourcesRequest{ namespace, r#type }`; the e2e seeds `ServiceStatusSpec` (the exact struct `resource_to_fields` maps) and asserts the `etcd` id surfaces.
- **Placeholder scan:** none; every step ships complete code + exact commands.
```
