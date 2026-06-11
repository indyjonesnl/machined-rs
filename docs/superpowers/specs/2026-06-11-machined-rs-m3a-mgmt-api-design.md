# machined-rs M3a — Management API (vertical slice), Design

**Date:** 2026-06-11
**Status:** Approved (brainstorming) — proceeds to two implementation plans
**Parent design:** `2026-06-10-machined-rs-design.md` (begins milestone M3: management gRPC API + CLI + PKI)
**Builds on:** M0–M2 (the full node-config pipeline + the COSI store), merged to `main`.

## 1. Overview

M3a makes the node remotely observable: a node PKI (rcgen CA + certs), a tonic gRPC server over
mutual TLS exposing read-only RPCs (`Version` + a generic `ListResources` over the COSI store), and a
minimal `machinectl` CLI that connects with a client certificate and queries the node. It is the
talosctl-equivalent vertical slice — proving PKI → mTLS → gRPC → CLI end-to-end. Actions (reboot,
etc.) and richer RPCs are M3b/M3c.

## 2. Goals / Non-goals

### Goals
- A `pki` crate: generate a node CA and CA-signed server/client certificates (rcgen, pure-Rust),
  load-or-generate from a directory.
- An `apiserver` crate: a clean-break `machine.proto` (tonic-build), a tonic mTLS server implementing
  `MachineService` (`Version`, `ListResources`) backed by a `runtime-core` `State` handle.
- A `machinectl` CLI: a tonic mTLS client presenting a CA-signed client cert, with `version` and
  `get <type>` subcommands.
- Wire it into `machined`: load-or-generate PKI at boot, spawn the server as a task, generate a client
  bundle `machinectl` can use.

### Non-goals (deferred)
- **All action/lifecycle RPCs** (Reboot, Shutdown, Reset, ApplyConfiguration, Bootstrap…) — M3c.
- **Streaming RPCs** (Logs, Events, Dmesg) and richer typed queries — M3b.
- Authorization/roles, API-signature auth, certificate rotation/expiry handling, multi-node/endpoint
  config — later.
- Running the server as a supervised service (it is a spawned task in M3a; supervising it is an M3b
  refinement).
- Persisting PKI to the encrypted STATE volume with proper permissions hardening (M3a persists to a
  plain directory; hardening is later).

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `pki` (new leaf) | rcgen CA + cert generation; `NodePki` load-or-generate. Depends on `rcgen` + `thiserror`. |
| `apiserver` (new) | `proto/machine.proto` + `build.rs` (tonic-build); the `MachineService` impl over `State`; the mTLS server builder. Depends on `tonic`, `prost`, `runtime-core`, `resources`, `pki`. |
| `machinectl` (new bin) | tonic mTLS client + `clap`-parsed subcommands. Depends on `tonic`, `prost`, the apiserver's generated client (re-exported), `pki` (to read the bundle), `tokio`. |
| `machined` | load-or-generate `NodePki` at boot; spawn the apiserver task with the `State`; write a `machinectl` client bundle. |

New workspace deps (versions matching the sibling `rusternetes`): `tonic = "0.12"`, `prost = "0.13"`,
`tonic-build = "0.12"` (build-dep), `rcgen = "0.13"`, `clap = "4"` (machinectl).

### 3.2 `pki` crate

```text
generate_ca() -> CertKey                      // self-signed CA (cert + key PEM)
generate_cert(ca: &CertKey, cn: &str, role: CertRole, sans: &[String]) -> CertKey
    CertRole = Server | Client                // sets the EKU (serverAuth / clientAuth)

CertKey { cert_pem: String, key_pem: String }

NodePki:
    load_or_generate(dir: &Path) -> NodePki    // ca.pem/ca.key + server.pem/server.key
        // generates + writes them if absent; loads them if present (idempotent boot)
    server_identity() -> (cert_pem, key_pem)
    ca_pem() -> String
    issue_client(cn: &str) -> CertKey          // a fresh client cert for machinectl
```

- Uses `rcgen` 0.13: a CA `Certificate` (is_ca), then leaf certs signed by it with the right EKU + SANs
  (the server cert's SAN is the node's address / `localhost` / `127.0.0.1`).
- All-PEM in/out so `tonic`/`rustls` can consume directly.

### 3.3 `apiserver` crate

`proto/machine.proto`:

```proto
syntax = "proto3";
package machine;

service MachineService {
  rpc Version(Empty) returns (VersionResponse);
  rpc ListResources(ListResourcesRequest) returns (ListResourcesResponse);
}

message Empty {}
message VersionResponse { string version = 1; }

message ListResourcesRequest { string namespace = 1; string type = 2; }
message KeyValue { string key = 1; string value = 2; }
message ResourceEntry { string id = 1; repeated KeyValue fields = 2; }
message ListResourcesResponse { repeated ResourceEntry entries = 1; }
```

`build.rs` runs `tonic_build::compile_protos("proto/machine.proto")`.

The service impl (`MachineService`) holds a `State`:
- `Version` → `{ version: env!("CARGO_PKG_VERSION") }` (the machined version, threaded in).
- `ListResources` → parse `type` to a `ResourceType` (a `parse_resource_type(&str)` reverse of
  `Display`); `state.list(namespace, typ)`; map each `ResourceObject` to a `ResourceEntry` via
  `resource_to_fields(&Resource) -> Vec<(String, String)>` — one exhaustive match over the closed
  `Resource` enum producing each variant's fields as strings. An unknown `type` string → a
  `Status::invalid_argument`.

The mTLS server builder:

```text
serve(addr, state, version, pki) -> impl Future
    tonic Server::builder()
        .tls_config(ServerTlsConfig::new()
            .identity(server cert/key from pki)
            .client_ca_root(pki.ca))   // require a client cert signed by the CA
        .add_service(MachineServiceServer::new(impl))
        .serve(addr)
```

### 3.4 `machinectl` CLI

`clap` subcommands:
- `machinectl --bundle <dir> version` → calls `Version`, prints it.
- `machinectl --bundle <dir> get <type> [--namespace <ns>]` → calls `ListResources`, prints a table
  (`ID` + one column per field key, or `key=value` rows).

Connects with a tonic `Channel` using `ClientTlsConfig` (the CA cert + the client identity read from
`<bundle>/ca.pem`, `<bundle>/client.pem`, `<bundle>/client.key`). Endpoint default
`https://127.0.0.1:50000` (`--endpoint` to override).

### 3.5 Wiring (`machined`)

In `run_daemon`: `NodePki::load_or_generate(pki_dir)`; write a client bundle (ca + a fresh client
cert) to a known dir for `machinectl`; `tokio::spawn(apiserver::serve(addr, state.clone(), VERSION,
pki))`. The server listens on `127.0.0.1:50000` (fixed for M3a; configurable later). It shares the same
`State` the controllers populate, so queries reflect live node state.

### 3.6 Plan decomposition

- **M3a-1:** `pki` + `apiserver` (proto, mTLS server, `Version`, `ListResources`), validated by a Rust
  integration test that starts the server on a loopback port with a generated PKI, connects the
  tonic-generated client with a client cert, seeds the `State`, and asserts `Version` + `ListResources`
  responses. Fully exercises the stack without the CLI.
- **M3a-2:** the `machinectl` CLI + `machined` wiring + an end-to-end test (spawn the server, run the
  CLI against it).

## 4. Error handling & observability

- `pki` has a `PkiError` (`thiserror`) wrapping rcgen + IO failures.
- The service maps store/parse failures to gRPC `Status` (`invalid_argument` for an unknown type,
  `internal` for store errors).
- The server task logs (via `tracing`) bind/serve errors; a serve failure logs but does not crash the
  daemon (the node stays up without the API).
- mTLS rejects an unauthenticated/unsigned client at the transport layer (tonic/rustls), before any
  RPC handler runs.

## 5. Testing strategy

- **Unit (root-free):**
  - `pki`: `generate_ca` produces a CA cert; `generate_cert(Client)`/`(Server)` produce leaves that
    chain to the CA (verify with `rustls`'s verifier or by re-parsing the issuer); `NodePki::
    load_or_generate` is idempotent (second call loads, doesn't regenerate — same CA).
  - `apiserver`: `parse_resource_type` round-trips `Display`; `resource_to_fields` covers each
    `Resource` variant (the closed enum guarantees exhaustiveness — a missing arm is a compile error).
- **Integration (root-free, M3a-1):** start `apiserver::serve` on `127.0.0.1:0` with a tempdir PKI;
  connect the tonic client with the client identity; seed the `State` with a couple of resources
  (e.g. a `ServiceStatus` + a `TimeStatus`); assert `Version` returns the version and `ListResources`
  returns the seeded resources' fields. Also assert a client with **no** cert (or a self-signed one) is
  rejected — proving mTLS is enforced.
- **Integration (root-free, M3a-2):** the e2e — `machined`-style wiring spawns the server, the
  `machinectl` binary (`assert_cmd` or invoking the built binary) runs `version` + `get ServiceStatus`
  against it and the output contains the expected data.
- **CI:** `make pre-commit` (the new crates build their proto via `tonic-build`, which needs `protoc`
  — note the dependency; if `protoc` is unavailable in CI, use the `prost`-vendored `protoc` or the
  `protoc-bin-vendored` path).

## 6. Key risks

- **`tonic-build` needs `protoc`.** Confirm `protoc` is available, or vendor it. This is the first
  build-time external tool in the project — the M3a-1 spike must resolve it (e.g. add
  `protoc-bin-vendored` and point `PROTOC` at it in `build.rs`, or document the system dependency).
- **`rcgen` 0.13 API** — cert/CA construction (`CertifiedKey`, `KeyPair`, `CertificateParams`, EKU,
  SANs) shifted across versions; spike the CA+leaf generation + a chain-verify early.
- **tonic mTLS wiring** — `ServerTlsConfig.identity`/`client_ca_root` and the client `ClientTlsConfig`
  must agree on PEM formats; the integration test (connect + reject-unauthenticated) is the acceptance
  proof.
- **Closed-enum field mapping** — `resource_to_fields` must handle every `Resource` variant; rely on
  the exhaustive match (no wildcard arm) so new resource types force an update.
- **Address/SAN** — the server cert SAN must include `127.0.0.1`/`localhost` so the loopback client
  validates the hostname; get this right in `pki` or the client will reject the cert.
