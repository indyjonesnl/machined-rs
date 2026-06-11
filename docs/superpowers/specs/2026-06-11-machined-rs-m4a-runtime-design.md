# machined-rs M4a — Container Runtime (containerd + CRI health), Design

**Date:** 2026-06-11
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (begins milestone M4: container runtime + payload)
**Builds on:** M0–M3 merged to `main` (supervisor, periodic reconcile, mgmt API).

## 1. Overview

M4a makes the node run a container runtime: machined launches **containerd as a built-in supervised
system service** and continuously verifies it is genuinely ready via a **CRI gRPC health probe** over
containerd's unix socket, publishing a `RuntimeStatus` resource. This is the foundation M4b's payload
bring-up gates on (health-gated `depends_on`), and the readiness is visible remotely via
`machinectl get RuntimeStatus`. machined stays **workload-agnostic**: it supervises the external
containerd binary; payloads (e.g. the rusternetes kubelet) are M4b config-declared services.

## 2. Goals / Non-goals

### Goals
- A `cri` crate: a **trimmed** CRI `runtime.proto` (RuntimeService `Version` + `Status` only),
  a `CriClient` trait, a `GrpcCriClient` (tonic over UDS), and a `FakeCriClient`.
- A `runtime { disabled, binary, socket, config_path }` machine-config section.
- A `RuntimeStatus { ready, name, version }` resource.
- A `RuntimeHealthController` probing CRI on the periodic timer and publishing `RuntimeStatus`.
- machined wiring: generate a minimal containerd `config.toml`, inject a built-in
  `containerd` `ServiceConfig` ahead of user services, register the health controller.

### Non-goals (deferred)
- **Payload bring-up** (config-declared services gated on runtime readiness, health-gated
  `depends_on`) — M4b.
- Image/pod/container CRI RPCs (pull, run, list) — machined never manages workloads; only the
  runtime's health. The trimmed proto can be extended if M4b+ ever needs more.
- Shipping/installing the containerd binary itself (it is part of the image/rootfs, not machined).
- containerd config beyond the minimal CRI-enabled stub; registry mirrors/auth; CNI configuration.
- Runtime restart-storm handling beyond the existing supervisor `RestartPolicy`.

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `cri` (new leaf) | `proto/runtime.proto` (trimmed, package `runtime.v1`) + tonic-build (vendored protoc); `CriClient` trait + `GrpcCriClient` (UDS) + `FakeCriClient`. Depends on tonic/prost/tokio/tower/async-trait/thiserror. |
| `config` | + `RuntimeSection` on `MachineSection` (+ the literal follow-through — grep `MachineSection {`). |
| `resources` | + `RuntimeStatus` + `ResourceType` variant (+ apiserver `resource_to_fields`/`parse_resource_type` arms — the closed enum forces both). |
| `controllers` | + `runtime::RuntimeHealthController`. |
| `machined` | containerd config.toml generation + built-in service injection + controller registration. |

### 3.2 `cri` crate

**Trimmed proto** (`proto/runtime.proto`): package `runtime.v1`, service `RuntimeService` with only

```proto
rpc Version(VersionRequest) returns (VersionResponse);
rpc Status(StatusRequest) returns (StatusResponse);
```

and the transitively required messages (`VersionRequest{version}`, `VersionResponse{version,
runtime_name, runtime_version, runtime_api_version}`, `StatusRequest{verbose}`,
`StatusResponse{status RuntimeStatus{conditions []RuntimeCondition{type, status, reason, message}}}`
— **field numbers copied exactly from the upstream k8s `cri-api` proto**; trimming removes messages,
never renumbers). The wire format must match real containerd.

```text
trait CriClient (Send + Sync):
    async fn version(&self) -> Result<RuntimeVersion>      // { runtime_name, runtime_version }
    async fn ready(&self) -> Result<bool>                  // Status: the RuntimeReady condition only

GrpcCriClient::connect(socket_path) -> Result<Self>
    // tonic Endpoint::from_static("http://[::]:0").connect_with_connector(service_fn(
    //     move |_| UnixStream::connect(path)))  — gRPC over the containerd UDS
FakeCriClient: configurable version/ready/error; records calls.
```

`ready()` returns true iff the `RuntimeReady` condition is true (NetworkReady is reported but NOT
required for M4a readiness — CNI is out of scope; the payload may bring its own network. Only
`RuntimeReady` gates).

### 3.3 Config: `runtime` section

```yaml
machine:
  runtime:
    disabled: false                                  # default false
    binary: /usr/bin/containerd                      # default
    socket: /run/containerd/containerd.sock          # default
    config_path: /etc/containerd/config.toml         # default (generated)
```

`RuntimeSection { disabled: bool, binary: String, socket: String, config_path: String }` with serde
defaults producing the values above (a custom `Default` impl + `#[serde(default)]`).

### 3.4 `RuntimeStatus` resource

```text
RuntimeStatus { ready: bool, name: String, version: String }
```

Namespace `runtime`, singleton id `containerd`. Mapped in the apiserver (`resource_to_fields` +
`parse_resource_type` gain arms — compile-enforced), so `machinectl get RuntimeStatus` works
immediately.

### 3.5 `RuntimeHealthController`

- Inputs: none. `resync_interval() -> Some(10s)`.
- Output: `RuntimeStatus` (Exclusive, via `reconcile_owned`).
- Reconcile: if `runtime.disabled` → publish `ready:false, name:"", version:""` and return. Else
  `client.version()` + `client.ready()`; publish `RuntimeStatus { ready, name, version }`. An
  unreachable socket is **transient**: warn + publish `ready:false` + return `Ok` (the 10s tick
  retries) — same posture as time sync. The controller holds an `Arc<dyn CriClient>`.
- The `GrpcCriClient` connects lazily/per-probe (the socket may not exist until containerd is up);
  a connect failure is the unreachable case.

### 3.6 machined wiring

Unless `runtime.disabled` (steps 1–2 live in the sequencer's StartServices boot task — the natural
home for boot actions; step 3 in machined's `run_daemon`):
1. **Generate** the minimal containerd config at `config_path` (create parent dirs; only if absent —
   the file is owned by machined but user-replaceable by baking their own into the image):

```toml
version = 2
[grpc]
  address = "<socket>"
[plugins."io.containerd.grpc.v1.cri"]
```

2. **Inject** a built-in service ahead of the user services handed to the supervisor:
   `ServiceConfig { id: "containerd", command: [binary, "--config", config_path], depends_on: [],
   restart: Always }`. User services may `depends_on: [containerd]` (start-ordering today; health
   gating is M4b). A user-declared service id `containerd` is rejected (config validation error).
3. **Register** `RuntimeHealthController` with a `GrpcCriClient` against `runtime.socket` (Linux;
   fake otherwise).

## 4. Error handling & observability

- `cri` has a `CriError` (connect/transport/RPC), `thiserror`.
- Probe failures are transient warnings; `RuntimeStatus.ready:false` is the observable signal.
- The injected containerd service gets the existing supervisor restart + `ServiceStatus` machinery.
- `machinectl get RuntimeStatus` (and `get ServiceStatus`) expose the runtime state remotely.

## 5. Testing strategy

- **Unit (root-free):** config parse/defaults; `RuntimeStatus` resource; `FakeCriClient`; controller
  against the fake (ready, not-ready, disabled, error→`ready:false`+Ok); the service-injection +
  config.toml generation as pure functions (`containerd_service(cfg) -> ServiceConfig`,
  `containerd_config_toml(cfg) -> String`, id-collision validation).
- **Integration (root-free):** a **fake CRI server on a UnixListener** (tonic `serve_with_incoming`)
  implementing the trimmed proto; the real `GrpcCriClient` connects over the UDS and asserts
  version/ready — proving the UDS connector + wire format end-to-end without containerd.
- **Integration (gated):** `#[ignore]`d test against a real containerd socket (needs containerd
  running) — validates the trimmed proto against the real implementation. Manual/CI-with-containerd.
- **e2e (root-free):** runtime + `RuntimeHealthController` + fake CRI UDS server → `RuntimeStatus`
  appears in the store with `ready:true`.
- **CI:** `make pre-commit`.

## 6. Key risks

- **Trimmed-proto wire compatibility** — field numbers/types must match upstream `cri-api` exactly;
  the UDS fake-server test proves self-consistency but only the gated real-containerd test proves
  upstream compatibility. Copy the message definitions verbatim from `k8s.io/cri-api`
  (`runtime/v1/api.proto`) and trim whole messages only.
- **tonic-over-UDS connector** — `connect_with_connector` + `service_fn`/`UnixStream` API shape varies
  across tonic/tower/hyper versions (tonic 0.12 needs the hyper-util `TokioIo` wrapper). Spike: the
  UDS fake-server test is the acceptance gate.
- **Socket-not-yet-present races** — containerd creates its socket after start; the controller must
  treat connect-failure as transient (it does) and the 10s resync makes readiness eventually-true.
  No boot-blocking wait in M4a (M4b's health-gated depends_on handles ordering).
- **Service-id collision** — the injected `containerd` id must be reserved; config validation rejects
  a user service with that id (clear error at load).
