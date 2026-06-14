# M8a — machined Runs a Pod via CRI: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** machined orchestrates a real container via CRI — extend the CRI client to the pod lifecycle, add a `PodController` that runs config-declared pods and publishes `PodStatus`, set up cgroup-v2 delegation + the overlay module + pre-baked images, and assert `PodStatus=Running` in the x86_64 boot test.

**Architecture:** A new `PodController` depends ONLY on the `CriClient` trait (CRI v1 proto), keeping the runtime pluggable. containerd-specific bits (sandbox image, `ctr` image import) stay quarantined in `crates/config/src/runtime_svc.rs` + the daemon wiring. cgroup delegation is a privileged `Platform` op done in PID1 early boot. Images are pre-baked OCI archives staged to `/boot/images` and imported into containerd's `k8s.io` namespace at boot; the pod uses host networking (CNI is M8b).

**Tech Stack:** Rust, tonic/prost (CRI gRPC over the containerd unix socket), the existing COSI reconcile runtime, the imager pipeline, QEMU/KVM boot test.

**Pluggability guardrails (enforced in review):**
- `PodController` imports `machined_cri` (the trait) only — NEVER `containerd`/`ctr` directly.
- containerd specifics (`sandbox_image`, the `ctr images import` argv) live ONLY in `crates/config/src/runtime_svc.rs`.
- Pod/network config schema is CRI/CNI-shaped (name/image/command/args/host_network), no vendor fields.

**Operator handoff (like the GHCR CI image):** Task 11 adds the `oci-image` artifact mechanism + a build script that produces `pause.tar` + `busybox.tar` and prints their sha256. The two tarballs must be hosted (GHCR release asset under `indyjonesnl/machined-rs`) and their url+sha pasted into `artifacts.toml` before the Task 13 boot test goes green — a deliberate two-phase step, not a placeholder.

**Reference spec:** `docs/superpowers/specs/2026-06-14-machined-rs-m8-run-a-pod-design.md` (M8a section).

---

## File Structure

| File | Responsibility | Tasks |
|---|---|---|
| `crates/cri/proto/runtime.proto` | CRI wire types — add image/sandbox/container RPCs + messages | 1 |
| `crates/cri/src/lib.rs` | `CriClient` trait + domain structs (`PodSpec`, `ContainerSpec`, states) | 2,3,4 |
| `crates/cri/src/grpc.rs` | Real client: tonic impls of the new methods | 2,3,4 |
| `crates/cri/src/fake.rs` | In-memory CRI state machine for controller tests | 2,3,4 |
| `crates/resources/src/pod_status.rs` (new) | `PodStatus` spec (pure data) | 5 |
| `crates/resources/src/{lib,resource,metadata}.rs` | wire `PodStatus` into the closed enum + `ResourceType` | 5 |
| `crates/apiserver/src/mapping.rs` | render/parse `PodStatus` for `machinectl get` | 5 |
| `crates/config/src/types.rs` | `PodConfig` + `MachineSection.pods` | 6 |
| `crates/config/src/provider.rs` | `Provider::pods()` | 6 |
| `crates/controllers/src/runtime/pod.rs` (new) | `PodController` (CRI-trait-only) | 7 |
| `crates/controllers/src/runtime/mod.rs` | export `PodController` | 7 |
| `crates/platform/src/{lib,linux,fake}.rs` | cgroup delegation op + pure `subtree_control_line` | 8 |
| `crates/imager/src/modules.rs` | add `overlay` to `VIRT_MODULES` | 9 |
| `crates/config/src/runtime_svc.rs` | `sandbox_image` in the toml + `ctr_import_args` (quarantine) | 10 |
| `crates/imager/src/{manifest,build}.rs` | `oci-image` artifact kind → stage to `/boot/images` | 11 |
| `crates/imager/artifacts.toml` | pin pause + busybox OCI archives (x86_64) | 11 |
| `scripts/build-oci-images.sh` (new) | produce the two OCI archives + print sha256 | 11 |
| `crates/machined/src/main.rs` | wire cgroup delegation, image-import task, `PodController` | 12 |
| `crates/machined/src/runtime_images.rs` (new) | spawn the `ctr` image-import task (containerd-specific) | 12 |
| `examples/node-ci.yaml` | the `hello` pod | 13 |
| `scripts/boot-test-x86_64.sh` | assert `PodStatus name=hello phase=Running` | 13 |

---

## Task 1: Extend the CRI proto with the pod lifecycle

**Files:**
- Modify: `crates/cri/proto/runtime.proto`

The existing `Version`/`Status` RPCs and their messages stay verbatim. Add the image + sandbox + container RPCs and the message subset machined sets/reads. Field numbers are copied exactly from `k8s.io/cri-api` `runtime/v1/api.proto` for wire compatibility with containerd 2.x; only the fields machined uses are declared.

- [ ] **Step 1: Add the new RPCs to the two services**

In `runtime.proto`, replace the `service RuntimeService { ... }` block with:

```proto
service RuntimeService {
  rpc Version(VersionRequest) returns (VersionResponse) {}
  rpc Status(StatusRequest) returns (StatusResponse) {}
  rpc RunPodSandbox(RunPodSandboxRequest) returns (RunPodSandboxResponse) {}
  rpc ListPodSandbox(ListPodSandboxRequest) returns (ListPodSandboxResponse) {}
  rpc CreateContainer(CreateContainerRequest) returns (CreateContainerResponse) {}
  rpc StartContainer(StartContainerRequest) returns (StartContainerResponse) {}
  rpc ListContainers(ListContainersRequest) returns (ListContainersResponse) {}
  rpc ContainerStatus(ContainerStatusRequest) returns (ContainerStatusResponse) {}
}

service ImageService {
  rpc ImageStatus(ImageStatusRequest) returns (ImageStatusResponse) {}
  rpc PullImage(PullImageRequest) returns (PullImageResponse) {}
}
```

- [ ] **Step 2: Append the message + enum definitions**

Append to `runtime.proto` (after the existing `StatusResponse`):

```proto
// ---- shared ----
message PodSandboxMetadata { string name = 1; string uid = 2; string namespace = 3; uint32 attempt = 4; }
message ContainerMetadata { string name = 1; uint32 attempt = 2; }
message ImageSpec { string image = 1; }
message KeyValue { string key = 1; string value = 2; }

enum NamespaceMode { POD = 0; CONTAINER = 1; NODE = 2; TARGET = 3; }
message NamespaceOption { NamespaceMode network = 1; NamespaceMode pid = 2; NamespaceMode ipc = 3; }
message LinuxSandboxSecurityContext { NamespaceOption namespace_options = 1; }
message LinuxPodSandboxConfig { string cgroup_parent = 1; LinuxSandboxSecurityContext security_context = 2; }
message PodSandboxConfig {
  PodSandboxMetadata metadata = 1;
  string hostname = 2;
  string log_directory = 3;
  map<string, string> labels = 6;
  LinuxPodSandboxConfig linux = 8;
}

enum PodSandboxState { SANDBOX_READY = 0; SANDBOX_NOTREADY = 1; }
message PodSandboxStateValue { PodSandboxState state = 1; }
message PodSandboxFilter { string id = 1; PodSandboxStateValue state = 2; map<string, string> label_selector = 3; }
message PodSandbox { string id = 1; PodSandboxMetadata metadata = 2; PodSandboxState state = 3; map<string, string> labels = 5; }

enum ContainerState { CONTAINER_CREATED = 0; CONTAINER_RUNNING = 1; CONTAINER_EXITED = 2; CONTAINER_UNKNOWN = 3; }
message ContainerStateValue { ContainerState state = 1; }
message ContainerFilter { string id = 1; ContainerStateValue state = 2; string pod_sandbox_id = 3; map<string, string> label_selector = 4; }
message Container { string id = 1; string pod_sandbox_id = 2; ContainerMetadata metadata = 3; ImageSpec image = 4; ContainerState state = 6; map<string, string> labels = 8; }
message ContainerConfig {
  ContainerMetadata metadata = 1;
  ImageSpec image = 2;
  repeated string command = 3;
  repeated string args = 4;
  map<string, string> labels = 9;
}
message ContainerStatus { string id = 1; ContainerMetadata metadata = 2; ContainerState state = 3; int32 exit_code = 7; string reason = 10; string message = 11; }

// ---- requests / responses ----
message RunPodSandboxRequest { PodSandboxConfig config = 1; string runtime_handler = 2; }
message RunPodSandboxResponse { string pod_sandbox_id = 1; }
message ListPodSandboxRequest { PodSandboxFilter filter = 1; }
message ListPodSandboxResponse { repeated PodSandbox items = 1; }

message CreateContainerRequest { string pod_sandbox_id = 1; ContainerConfig config = 2; PodSandboxConfig sandbox_config = 3; }
message CreateContainerResponse { string container_id = 1; }
message StartContainerRequest { string container_id = 1; }
message StartContainerResponse {}
message ListContainersRequest { ContainerFilter filter = 1; }
message ListContainersResponse { repeated Container containers = 1; }
message ContainerStatusRequest { string container_id = 1; bool verbose = 2; }
message ContainerStatusResponse { ContainerStatus status = 1; }

message ImageStatusRequest { ImageSpec image = 1; bool verbose = 2; }
message Image { string id = 1; repeated string repo_tags = 2; repeated string repo_digests = 3; }
message ImageStatusResponse { Image image = 1; }
message PullImageRequest { ImageSpec image = 1; PodSandboxConfig sandbox_config = 3; }
message PullImageResponse { string image_ref = 1; }
```

- [ ] **Step 3: Verify codegen compiles**

Run: `cargo build -p machined-cri`
Expected: PASS (tonic regenerates `ImageServiceClient` + the new `RuntimeServiceClient` methods; existing code still compiles — the trait isn't touched yet).

- [ ] **Step 4: Commit**

```bash
git add crates/cri/proto/runtime.proto
git commit -m "feat(cri): vendor pod-lifecycle RPCs into the trimmed CRI proto"
```

---

## Task 2: CRI image service — `image_present` + `pull_image`

**Files:**
- Modify: `crates/cri/src/lib.rs`, `crates/cri/src/grpc.rs`, `crates/cri/src/fake.rs`

Add the first slice of the pod lifecycle: image presence (the controller's gate) and pull (the documented offline fallback). Every step keeps BOTH impls compiling.

- [ ] **Step 1: Write the failing fake test**

Append to the `tests` module in `crates/cri/src/fake.rs`:

```rust
#[tokio::test]
async fn fake_image_presence_and_pull() {
    let f = FakeCriClient::new().with_version("containerd", "2.0").with_image("busybox:1.36");
    assert!(f.image_present("busybox:1.36").await.unwrap());
    assert!(!f.image_present("nope:1").await.unwrap());
    // pull makes a previously-absent image present.
    f.pull_image("pulled:1").await.unwrap();
    assert!(f.image_present("pulled:1").await.unwrap());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-cri fake_image_presence_and_pull`
Expected: FAIL (`with_image`, `image_present`, `pull_image` undefined).

- [ ] **Step 3: Add the trait methods**

In `crates/cri/src/lib.rs`, add to the `CriClient` trait (after `ready`):

```rust
    /// True iff the image ref is present in the runtime's store (CRI ImageStatus).
    async fn image_present(&self, image: &str) -> Result<bool>;
    /// Pull an image by ref (CRI PullImage). Offline nodes pre-import instead;
    /// this is the fallback path when a registry is reachable.
    async fn pull_image(&self, image: &str) -> Result<()>;
```

- [ ] **Step 4: Implement on the real client**

In `crates/cri/src/grpc.rs`, add an image-service connector + the two methods. Add imports at the top:

```rust
use crate::pb::image_service_client::ImageServiceClient;
use crate::pb::{ImageSpec, ImageStatusRequest, PullImageRequest};
```

Add a second connect helper inside `impl GrpcCriClient` (mirrors `connect`, returns the image client):

```rust
    async fn connect_image(&self) -> Result<ImageServiceClient<Channel>> {
        let path = self.socket.clone();
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
        Ok(ImageServiceClient::new(channel))
    }
```

In the `impl CriClient for GrpcCriClient` block, add:

```rust
    async fn image_present(&self, image: &str) -> Result<bool> {
        let mut client = self.connect_image().await?;
        let resp = client
            .image_status(ImageStatusRequest {
                image: Some(ImageSpec { image: image.to_string() }),
                verbose: false,
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp.image.is_some())
    }

    async fn pull_image(&self, image: &str) -> Result<()> {
        let mut client = self.connect_image().await?;
        client
            .pull_image(PullImageRequest {
                image: Some(ImageSpec { image: image.to_string() }),
                sandbox_config: None,
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?;
        Ok(())
    }
```

- [ ] **Step 5: Implement on the fake**

In `crates/cri/src/fake.rs`, add `images` to `FakeState`:

```rust
#[derive(Default)]
struct FakeState {
    version: Option<RuntimeVersion>,
    ready: bool,
    calls: usize,
    images: std::collections::HashSet<String>,
}
```

Add the seed builder (after `with_ready`):

```rust
    pub fn with_image(self, image: &str) -> Self {
        self.state.lock().unwrap().images.insert(image.to_string());
        self
    }
```

Add to `impl CriClient for FakeCriClient`:

```rust
    async fn image_present(&self, image: &str) -> Result<bool> {
        Ok(self.state.lock().unwrap().images.contains(image))
    }
    async fn pull_image(&self, image: &str) -> Result<()> {
        self.state.lock().unwrap().images.insert(image.to_string());
        Ok(())
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p machined-cri`
Expected: PASS (the new test + the existing `fake_round_trip`).

- [ ] **Step 7: Commit**

```bash
git add crates/cri/src
git commit -m "feat(cri): image_present + pull_image on the CriClient trait"
```

---

## Task 3: CRI pod sandbox — `run_pod_sandbox` + `find_sandbox`

**Files:**
- Modify: `crates/cri/src/lib.rs`, `crates/cri/src/grpc.rs`, `crates/cri/src/fake.rs`

Add sandbox creation (host-network) and idempotent lookup-by-name. Introduce the `PodSpec` domain struct so the trait surface stays vendor-neutral (callers never touch `pb::` types).

- [ ] **Step 1: Write the failing fake test**

Append to `crates/cri/src/fake.rs` tests:

```rust
#[tokio::test]
async fn fake_sandbox_create_is_idempotent_by_name() {
    let f = FakeCriClient::new().with_version("containerd", "2.0");
    let spec = crate::PodSpec { name: "hello".into(), uid: "u-hello".into(), host_network: true };
    assert!(f.find_sandbox("hello").await.unwrap().is_none());
    let id = f.run_pod_sandbox(&spec).await.unwrap();
    assert_eq!(f.find_sandbox("hello").await.unwrap().as_deref(), Some(id.as_str()));
    assert_eq!(f.sandbox_count(), 1);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-cri fake_sandbox_create_is_idempotent_by_name`
Expected: FAIL (`PodSpec`, `run_pod_sandbox`, `find_sandbox` undefined).

- [ ] **Step 3: Add the domain struct + trait methods**

In `crates/cri/src/lib.rs`, add near `RuntimeVersion`:

```rust
/// What machined needs to start a pod sandbox. Vendor-neutral (no pb:: types).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodSpec {
    pub name: String,
    pub uid: String,
    /// true → the sandbox shares the node network namespace (no CNI).
    pub host_network: bool,
}
```

Add to the `CriClient` trait:

```rust
    /// Create a pod sandbox; returns its id (CRI RunPodSandbox).
    async fn run_pod_sandbox(&self, pod: &PodSpec) -> Result<String>;
    /// Find a READY sandbox whose metadata name == `name`; the labelled id, if any.
    async fn find_sandbox(&self, name: &str) -> Result<Option<String>>;
```

- [ ] **Step 4: Implement on the real client**

In `crates/cri/src/grpc.rs`, extend the `use crate::pb::{...}` import with:

```rust
use crate::pb::{
    LinuxPodSandboxConfig, LinuxSandboxSecurityContext, ListPodSandboxRequest, NamespaceMode,
    NamespaceOption, PodSandboxConfig, PodSandboxFilter, PodSandboxMetadata, RunPodSandboxRequest,
};
```

Add to `impl CriClient for GrpcCriClient`. The sandbox is labelled `io.machined.pod=<name>` so `find_sandbox` can locate it idempotently:

```rust
    async fn run_pod_sandbox(&self, pod: &crate::PodSpec) -> Result<String> {
        let mut client = self.connect().await?;
        let net = if pod.host_network { NamespaceMode::Node } else { NamespaceMode::Pod } as i32;
        let cfg = PodSandboxConfig {
            metadata: Some(PodSandboxMetadata {
                name: pod.name.clone(),
                uid: pod.uid.clone(),
                namespace: "default".into(),
                attempt: 0,
            }),
            hostname: pod.name.clone(),
            log_directory: String::new(),
            labels: std::collections::HashMap::from([("io.machined.pod".to_string(), pod.name.clone())]),
            linux: Some(LinuxPodSandboxConfig {
                cgroup_parent: String::new(),
                security_context: Some(LinuxSandboxSecurityContext {
                    namespace_options: Some(NamespaceOption { network: net, pid: 0, ipc: 0 }),
                }),
            }),
        };
        let resp = client
            .run_pod_sandbox(RunPodSandboxRequest { config: Some(cfg), runtime_handler: String::new() })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp.pod_sandbox_id)
    }

    async fn find_sandbox(&self, name: &str) -> Result<Option<String>> {
        let mut client = self.connect().await?;
        let resp = client
            .list_pod_sandbox(ListPodSandboxRequest {
                filter: Some(PodSandboxFilter {
                    id: String::new(),
                    state: None,
                    label_selector: std::collections::HashMap::from([("io.machined.pod".to_string(), name.to_string())]),
                }),
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp.items.into_iter().next().map(|s| s.id))
    }
```

- [ ] **Step 5: Implement on the fake**

In `crates/cri/src/fake.rs`, add a sandbox vec + counter to `FakeState`:

```rust
    next_id: usize,
    sandboxes: Vec<(String, String)>, // (id, name)
```

Add a test-inspection method on `FakeCriClient`:

```rust
    pub fn sandbox_count(&self) -> usize {
        self.state.lock().unwrap().sandboxes.len()
    }
```

Add to `impl CriClient for FakeCriClient`:

```rust
    async fn run_pod_sandbox(&self, pod: &crate::PodSpec) -> Result<String> {
        let mut s = self.state.lock().unwrap();
        s.next_id += 1;
        let id = format!("sandbox-{}", s.next_id);
        s.sandboxes.push((id.clone(), pod.name.clone()));
        Ok(id)
    }
    async fn find_sandbox(&self, name: &str) -> Result<Option<String>> {
        Ok(self.state.lock().unwrap().sandboxes.iter().find(|(_, n)| n == name).map(|(id, _)| id.clone()))
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p machined-cri`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/cri/src
git commit -m "feat(cri): run_pod_sandbox (host-net) + idempotent find_sandbox"
```

---

## Task 4: CRI containers — create/start/find/state

**Files:**
- Modify: `crates/cri/src/lib.rs`, `crates/cri/src/grpc.rs`, `crates/cri/src/fake.rs`

Add container creation, start, idempotent lookup, and state read. Introduce `ContainerSpec` + a vendor-neutral `ContainerState`.

- [ ] **Step 1: Write the failing fake test**

Append to `crates/cri/src/fake.rs` tests:

```rust
#[tokio::test]
async fn fake_container_lifecycle() {
    use crate::{ContainerSpec, ContainerState, PodSpec};
    let f = FakeCriClient::new().with_version("containerd", "2.0").with_image("busybox:1.36");
    let sb = f.run_pod_sandbox(&PodSpec { name: "hello".into(), uid: "u".into(), host_network: true }).await.unwrap();
    let cspec = ContainerSpec {
        name: "hello".into(),
        image: "busybox:1.36".into(),
        command: vec!["/bin/sh".into(), "-c".into()],
        args: vec!["sleep 3600".into()],
    };
    assert!(f.find_container(&sb, "hello").await.unwrap().is_none());
    let id = f.create_container(&sb, &cspec).await.unwrap();
    assert_eq!(f.container_state(&id).await.unwrap(), ContainerState::Created);
    f.start_container(&id).await.unwrap();
    assert_eq!(f.container_state(&id).await.unwrap(), ContainerState::Running);
    assert_eq!(f.find_container(&sb, "hello").await.unwrap().as_deref(), Some(id.as_str()));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-cri fake_container_lifecycle`
Expected: FAIL (`ContainerSpec`, `ContainerState`, the four methods undefined).

- [ ] **Step 3: Add the domain types + trait methods**

In `crates/cri/src/lib.rs` add:

```rust
/// What machined needs to create a container in a sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub args: Vec<String>,
}

/// Vendor-neutral container state (maps CRI ContainerState).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Exited,
    Unknown,
}
```

Add to the `CriClient` trait:

```rust
    /// Create a container in a sandbox; returns its id (CRI CreateContainer).
    async fn create_container(&self, sandbox_id: &str, c: &ContainerSpec) -> Result<String>;
    /// Start a created container (CRI StartContainer).
    async fn start_container(&self, container_id: &str) -> Result<()>;
    /// Find a container by metadata name within a sandbox; its id, if any.
    async fn find_container(&self, sandbox_id: &str, name: &str) -> Result<Option<String>>;
    /// Read a container's current state (CRI ContainerStatus).
    async fn container_state(&self, container_id: &str) -> Result<ContainerState>;
```

- [ ] **Step 4: Implement on the real client**

In `crates/cri/src/grpc.rs`, extend the `use crate::pb::{...}` import with:

```rust
use crate::pb::{
    ContainerConfig, ContainerFilter, ContainerMetadata, ContainerState as PbContainerState,
    ContainerStatusRequest, CreateContainerRequest, ImageSpec, ListContainersRequest,
    StartContainerRequest,
};
```

(`ContainerFilter.state` is `Option<ContainerStateValue>` set to `None`, and `resp.containers` yields `Container`s whose `.id` we read — neither type needs naming beyond what prost generates, so don't import them.)

Add a mapping helper (free function in `grpc.rs`):

```rust
fn map_state(pb: i32) -> crate::ContainerState {
    match PbContainerState::try_from(pb) {
        Ok(PbContainerState::ContainerCreated) => crate::ContainerState::Created,
        Ok(PbContainerState::ContainerRunning) => crate::ContainerState::Running,
        Ok(PbContainerState::ContainerExited) => crate::ContainerState::Exited,
        _ => crate::ContainerState::Unknown,
    }
}
```

Add to `impl CriClient for GrpcCriClient`. The container is labelled `io.machined.container=<name>` for idempotent lookup, and the create call needs the sandbox config echoed back (host-network sandbox config, minimal):

```rust
    async fn create_container(&self, sandbox_id: &str, c: &crate::ContainerSpec) -> Result<String> {
        let mut client = self.connect().await?;
        let config = ContainerConfig {
            metadata: Some(ContainerMetadata { name: c.name.clone(), attempt: 0 }),
            image: Some(ImageSpec { image: c.image.clone() }),
            command: c.command.clone(),
            args: c.args.clone(),
            labels: std::collections::HashMap::from([("io.machined.container".to_string(), c.name.clone())]),
        };
        let resp = client
            .create_container(CreateContainerRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                config: Some(config),
                sandbox_config: None,
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp.container_id)
    }

    async fn start_container(&self, container_id: &str) -> Result<()> {
        let mut client = self.connect().await?;
        client
            .start_container(StartContainerRequest { container_id: container_id.to_string() })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?;
        Ok(())
    }

    async fn find_container(&self, sandbox_id: &str, name: &str) -> Result<Option<String>> {
        let mut client = self.connect().await?;
        let resp = client
            .list_containers(ListContainersRequest {
                filter: Some(ContainerFilter {
                    id: String::new(),
                    state: None,
                    pod_sandbox_id: sandbox_id.to_string(),
                    label_selector: std::collections::HashMap::from([("io.machined.container".to_string(), name.to_string())]),
                }),
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp.containers.into_iter().next().map(|c| c.id))
    }

    async fn container_state(&self, container_id: &str) -> Result<crate::ContainerState> {
        let mut client = self.connect().await?;
        let resp = client
            .container_status(ContainerStatusRequest { container_id: container_id.to_string(), verbose: false })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp.status.map(|s| map_state(s.state)).unwrap_or(crate::ContainerState::Unknown))
    }
```

- [ ] **Step 5: Implement on the fake**

In `crates/cri/src/fake.rs`, add to `FakeState`:

```rust
    // (id, sandbox_id, name, running)
    containers: Vec<(String, String, String, bool)>,
```

Add to `impl CriClient for FakeCriClient`:

```rust
    async fn create_container(&self, sandbox_id: &str, c: &crate::ContainerSpec) -> Result<String> {
        let mut s = self.state.lock().unwrap();
        s.next_id += 1;
        let id = format!("ctr-{}", s.next_id);
        s.containers.push((id.clone(), sandbox_id.to_string(), c.name.clone(), false));
        Ok(id)
    }
    async fn start_container(&self, container_id: &str) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        if let Some(c) = s.containers.iter_mut().find(|c| c.0 == container_id) {
            c.3 = true;
        }
        Ok(())
    }
    async fn find_container(&self, sandbox_id: &str, name: &str) -> Result<Option<String>> {
        Ok(self.state.lock().unwrap().containers.iter()
            .find(|c| c.1 == sandbox_id && c.2 == name).map(|c| c.0.clone()))
    }
    async fn container_state(&self, container_id: &str) -> Result<crate::ContainerState> {
        let s = self.state.lock().unwrap();
        Ok(match s.containers.iter().find(|c| c.0 == container_id) {
            Some(c) if c.3 => crate::ContainerState::Running,
            Some(_) => crate::ContainerState::Created,
            None => crate::ContainerState::Unknown,
        })
    }
```

- [ ] **Step 6: Run the tests + clippy**

Run: `cargo test -p machined-cri && cargo clippy -p machined-cri --all-targets -- -D warnings`
Expected: PASS, no warnings (remove any unused `pb::` imports clippy flags).

- [ ] **Step 7: Commit**

```bash
git add crates/cri/src
git commit -m "feat(cri): container create/start/find/state on the CriClient trait"
```

---

## Task 5: `PodStatus` resource + apiserver mapping

**Files:**
- Create: `crates/resources/src/pod_status.rs`
- Modify: `crates/resources/src/lib.rs`, `crates/resources/src/resource.rs`, `crates/resources/src/metadata.rs`, `crates/apiserver/src/mapping.rs`

Adding a closed-enum variant forces every exhaustive match to handle it — that is the intended safety. Touch all five sites.

- [ ] **Step 1: Write the failing test**

Create `crates/resources/src/pod_status.rs`:

```rust
//! Observed state of a machined-run pod. Pure data.

/// Lifecycle phase of a pod machined runs via CRI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PodPhase {
    Pending,
    Running,
    Failed,
}

/// Observed state of one configured pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodStatus {
    pub name: String,
    pub phase: PodPhase,
    pub container_id: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_running() {
        let p = PodStatus {
            name: "hello".into(),
            phase: PodPhase::Running,
            container_id: "ctr-1".into(),
            message: String::new(),
        };
        assert_eq!(p.phase, PodPhase::Running);
    }
}
```

- [ ] **Step 2: Wire it into the resources crate**

In `crates/resources/src/lib.rs`: add `pub mod pod_status;` (next to the other `pub mod` lines) and re-export:

```rust
pub use pod_status::{PodPhase, PodStatus};
```

In `crates/resources/src/metadata.rs`, add `PodStatus` to the `ResourceType` enum (after `RuntimeStatus`):

```rust
    RuntimeStatus,
    PodStatus,
```

…and to the `Display` match:

```rust
            ResourceType::RuntimeStatus => "RuntimeStatus",
            ResourceType::PodStatus => "PodStatus",
```

In `crates/resources/src/resource.rs`: add the import `use crate::pod_status::PodStatus;`, add the variant to the `Resource` enum (after `RuntimeStatus(RuntimeStatus)`):

```rust
    RuntimeStatus(RuntimeStatus),
    PodStatus(PodStatus),
```

…and the arm in `resource_type()`:

```rust
            Resource::RuntimeStatus(_) => ResourceType::RuntimeStatus,
            Resource::PodStatus(_) => ResourceType::PodStatus,
```

- [ ] **Step 3: Wire it into the apiserver mapping**

In `crates/apiserver/src/mapping.rs`, add to `parse_resource_type`:

```rust
        "RuntimeStatus" => ResourceType::RuntimeStatus,
        "PodStatus" => ResourceType::PodStatus,
```

…and to `resource_to_fields` (after the `RuntimeStatus` arm):

```rust
        Resource::PodStatus(p) => vec![
            kv("name", &p.name),
            kv("phase", format!("{:?}", p.phase)),
            kv("container_id", &p.container_id),
            kv("message", &p.message),
        ],
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p machined-resources -p machined-apiserver`
Expected: PASS (the new unit test + the existing `type_name_round_trips_display`; the exhaustive matches now compile).

- [ ] **Step 5: Commit**

```bash
git add crates/resources/src crates/apiserver/src/mapping.rs
git commit -m "feat(resources): PodStatus resource + apiserver field mapping"
```

---

## Task 6: `pods` config schema

**Files:**
- Modify: `crates/config/src/types.rs`, `crates/config/src/provider.rs`

CRI-shaped pod declaration. No vendor fields.

- [ ] **Step 1: Write the failing test**

Append to the bottom of `crates/config/src/types.rs` (new `#[cfg(test)]` block, or extend if one exists — `types.rs` has none, so add):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pods_section() {
        let cfg: MachineConfig = serde_yaml::from_str(
            r#"
machine:
  pods:
    - name: hello
      image: docker.io/library/busybox:1.36
      command: ["/bin/sh", "-c"]
      args: ["echo ok; sleep 3600"]
      host_network: true
"#,
        )
        .unwrap();
        let pods = &cfg.machine.pods;
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].name, "hello");
        assert_eq!(pods[0].image, "docker.io/library/busybox:1.36");
        assert!(pods[0].host_network);
        assert_eq!(pods[0].args, vec!["echo ok; sleep 3600".to_string()]);
    }

    #[test]
    fn pods_default_empty_and_host_network_defaults_false() {
        let cfg: MachineConfig = serde_yaml::from_str(
            "machine:\n  pods:\n    - name: p\n      image: img\n",
        )
        .unwrap();
        assert!(!cfg.machine.pods[0].host_network);
        assert!(cfg.machine.pods[0].command.is_empty());
    }
}
```

`serde_yaml` is already a dev-dependency of the config crate if the loader tests use it; if not, add `serde_yaml` under `[dev-dependencies]` in `crates/config/Cargo.toml` (the workspace already pins it — use `serde_yaml.workspace = true`).

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-config parses_pods_section`
Expected: FAIL (`pods` field + `PodConfig` undefined).

- [ ] **Step 3: Add `PodConfig` + the section field**

In `crates/config/src/types.rs`, add the field to `MachineSection` (after `runtime`):

```rust
    /// Container runtime (containerd) management.
    #[serde(default)]
    pub runtime: RuntimeSection,
    /// Pods machined runs via CRI once the runtime is ready.
    #[serde(default)]
    pub pods: Vec<PodConfig>,
```

Add the struct (after `ServiceConfig`):

```rust
/// A pod machined runs via CRI. CRI/CNI-shaped — no runtime-vendor fields.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodConfig {
    /// Pod + container name (used as the CRI metadata name + PodStatus id).
    pub name: String,
    /// Image ref (must be present in the runtime store; pre-imported on offline nodes).
    pub image: String,
    /// argv[0..] for the container entrypoint.
    #[serde(default)]
    pub command: Vec<String>,
    /// argv tail.
    #[serde(default)]
    pub args: Vec<String>,
    /// Share the node network namespace (M8a: the only supported mode; CNI is M8b).
    #[serde(default)]
    pub host_network: bool,
}
```

- [ ] **Step 4: Add the provider accessor**

In `crates/config/src/provider.rs`, add the import to the `use crate::types::{...}` line (`PodConfig`) and the accessor:

```rust
    pub fn pods(&self) -> &[PodConfig] {
        &self.config.machine.pods
    }
```

Also re-export `PodConfig` in `crates/config/src/lib.rs` `pub use types::{...}` list.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p machined-config`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/config/src crates/config/Cargo.toml
git commit -m "feat(config): pods section (CRI-shaped PodConfig)"
```

---

## Task 7: `PodController`

**Files:**
- Create: `crates/controllers/src/runtime/pod.rs`
- Modify: `crates/controllers/src/runtime/mod.rs`

Reconciles config pods → CRI sandbox → container → started, gated on `RuntimeStatus.ready`, publishing `PodStatus`. Depends ONLY on the `CriClient` trait (pluggability guardrail).

- [ ] **Step 1: Write the failing tests**

Create `crates/controllers/src/runtime/pod.rs`:

```rust
//! Reconciles config-declared pods via CRI and publishes PodStatus.
//! Depends only on the CriClient trait — runtime-pluggable by construction.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use machined_config::Provider;
use machined_cri::{ContainerSpec, ContainerState, CriClient, PodSpec};
use machined_resources::{
    Key, PodPhase, PodStatus, Resource, ResourceObject, ResourceType, RuntimeStatus,
};
use machined_runtime_core::{reconcile_owned, Controller, Input, InputKind, Output, OutputKind, ReconcileCtx};
use tracing::warn;

use super::NS;

const OWNER: &str = "pod-controller";

pub struct PodController {
    cri: Arc<dyn CriClient>,
    provider: Provider,
}

impl PodController {
    pub fn new(cri: Arc<dyn CriClient>, provider: Provider) -> Self {
        Self { cri, provider }
    }
}

fn status_obj(name: &str, phase: PodPhase, container_id: &str, message: &str) -> ResourceObject {
    ResourceObject::new(
        NS,
        name,
        Resource::PodStatus(PodStatus {
            name: name.to_string(),
            phase,
            container_id: container_id.to_string(),
            message: message.to_string(),
        }),
    )
}

fn runtime_ready(state: &machined_runtime_core::State) -> bool {
    matches!(
        state.get(&Key::new(NS, ResourceType::RuntimeStatus, "containerd")).map(|o| o.spec),
        Ok(Resource::RuntimeStatus(RuntimeStatus { ready: true, .. }))
    )
}

#[async_trait]
impl Controller for PodController {
    fn name(&self) -> &str { OWNER }

    fn inputs(&self) -> Vec<Input> {
        vec![Input { namespace: NS.into(), typ: ResourceType::RuntimeStatus, kind: InputKind::Weak }]
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output { typ: ResourceType::PodStatus, kind: OutputKind::Exclusive }]
    }

    fn resync_interval(&self) -> Option<Duration> { Some(Duration::from_secs(5)) }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let pods = self.provider.pods();
        if pods.is_empty() {
            // GC any stale PodStatus we previously owned.
            reconcile_owned(&ctx.state, OWNER, NS, ResourceType::PodStatus, vec![])?;
            return Ok(());
        }
        let ready = runtime_ready(&ctx.state);
        let mut desired = Vec::with_capacity(pods.len());
        for p in pods {
            if !ready {
                desired.push(status_obj(&p.name, PodPhase::Pending, "", "runtime not ready"));
                continue;
            }
            desired.push(self.run_one(p).await);
        }
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::PodStatus, desired)?;
        Ok(())
    }
}

impl PodController {
    async fn run_one(&self, p: &machined_config::PodConfig) -> ResourceObject {
        // 1. image must be present (pre-imported on offline nodes).
        match self.cri.image_present(&p.image).await {
            Ok(true) => {}
            Ok(false) => return status_obj(&p.name, PodPhase::Pending, "", "image not present"),
            Err(e) => {
                warn!(pod = %p.name, error = %e, "image_present failed");
                return status_obj(&p.name, PodPhase::Pending, "", "cri unreachable");
            }
        }
        // 2. sandbox (idempotent by name).
        let sandbox = match self.ensure_sandbox(p).await {
            Ok(id) => id,
            Err(m) => return status_obj(&p.name, PodPhase::Pending, "", &m),
        };
        // 3. container (idempotent by name within the sandbox).
        let container = match self.ensure_container(&sandbox, p).await {
            Ok(id) => id,
            Err(m) => return status_obj(&p.name, PodPhase::Pending, "", &m),
        };
        // 4. observe + report.
        match self.cri.container_state(&container).await {
            Ok(ContainerState::Running) => status_obj(&p.name, PodPhase::Running, &container, ""),
            Ok(ContainerState::Exited) => status_obj(&p.name, PodPhase::Failed, &container, "container exited"),
            Ok(_) => status_obj(&p.name, PodPhase::Pending, &container, "starting"),
            Err(e) => status_obj(&p.name, PodPhase::Pending, &container, &e.to_string()),
        }
    }

    async fn ensure_sandbox(&self, p: &machined_config::PodConfig) -> std::result::Result<String, String> {
        if let Some(id) = self.cri.find_sandbox(&p.name).await.map_err(|e| e.to_string())? {
            return Ok(id);
        }
        let spec = PodSpec { name: p.name.clone(), uid: format!("uid-{}", p.name), host_network: p.host_network };
        self.cri.run_pod_sandbox(&spec).await.map_err(|e| e.to_string())
    }

    async fn ensure_container(&self, sandbox: &str, p: &machined_config::PodConfig) -> std::result::Result<String, String> {
        if let Some(id) = self.cri.find_container(sandbox, &p.name).await.map_err(|e| e.to_string())? {
            // start if still in Created.
            if matches!(self.cri.container_state(&id).await, Ok(ContainerState::Created)) {
                self.cri.start_container(&id).await.map_err(|e| e.to_string())?;
            }
            return Ok(id);
        }
        let cspec = ContainerSpec {
            name: p.name.clone(),
            image: p.image.clone(),
            command: p.command.clone(),
            args: p.args.clone(),
        };
        let id = self.cri.create_container(sandbox, &cspec).await.map_err(|e| e.to_string())?;
        self.cri.start_container(&id).await.map_err(|e| e.to_string())?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::{MachineConfig, MachineSection, PodConfig};
    use machined_cri::FakeCriClient;
    use machined_runtime_core::State;

    fn provider_with_pod(host_network: bool) -> Provider {
        Provider::new(MachineConfig {
            machine: MachineSection {
                pods: vec![PodConfig {
                    name: "hello".into(),
                    image: "busybox:1.36".into(),
                    command: vec!["/bin/sh".into(), "-c".into()],
                    args: vec!["sleep 3600".into()],
                    host_network,
                }],
                ..Default::default()
            },
        })
    }

    fn pod_status(state: &State, name: &str) -> PodStatus {
        match state.get(&Key::new(NS, ResourceType::PodStatus, name)).unwrap().spec {
            Resource::PodStatus(p) => p,
            _ => panic!("wrong type"),
        }
    }

    fn mark_ready(state: &State) {
        let _ = state.create(ResourceObject::new(
            NS, "containerd",
            Resource::RuntimeStatus(RuntimeStatus { ready: true, name: "containerd".into(), version: "2".into() }),
        ));
    }

    #[tokio::test]
    async fn pending_when_runtime_not_ready() {
        let cri = Arc::new(FakeCriClient::new().with_version("c", "2").with_image("busybox:1.36"));
        let state = State::new();
        let ctx = ReconcileCtx { state: state.clone() };
        let mut c = PodController::new(cri, provider_with_pod(true));
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(pod_status(&state, "hello").phase, PodPhase::Pending);
    }

    #[tokio::test]
    async fn runs_pod_when_ready_and_image_present() {
        let cri = Arc::new(FakeCriClient::new().with_version("c", "2").with_image("busybox:1.36"));
        let state = State::new();
        mark_ready(&state);
        let ctx = ReconcileCtx { state: state.clone() };
        let mut c = PodController::new(cri.clone(), provider_with_pod(true));
        c.reconcile(&ctx).await.unwrap();
        let st = pod_status(&state, "hello");
        assert_eq!(st.phase, PodPhase::Running);
        assert!(!st.container_id.is_empty());
        assert_eq!(cri.sandbox_count(), 1);
        // Idempotent: a second reconcile must not create a second sandbox.
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(cri.sandbox_count(), 1);
    }

    #[tokio::test]
    async fn pending_when_image_absent() {
        let cri = Arc::new(FakeCriClient::new().with_version("c", "2")); // no image
        let state = State::new();
        mark_ready(&state);
        let ctx = ReconcileCtx { state: state.clone() };
        let mut c = PodController::new(cri, provider_with_pod(true));
        c.reconcile(&ctx).await.unwrap();
        let st = pod_status(&state, "hello");
        assert_eq!(st.phase, PodPhase::Pending);
        assert_eq!(st.message, "image not present");
    }
}
```

`MachineSection` must support `..Default::default()` — it already derives `Default`. The new `pods` field defaults to empty via that derive.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-controllers runs_pod_when_ready`
Expected: FAIL (module not yet exported / referenced).

- [ ] **Step 3: Export the controller**

In `crates/controllers/src/runtime/mod.rs`:

```rust
pub mod health;
pub mod pod;

pub use health::RuntimeHealthController;
pub use pod::PodController;
```

- [ ] **Step 4: Run the tests + clippy**

Run: `cargo test -p machined-controllers && cargo clippy -p machined-controllers --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/controllers/src/runtime
git commit -m "feat(controllers): PodController runs config pods via CRI (trait-only)"
```

---

## Task 8: cgroup-v2 controller delegation

**Files:**
- Modify: `crates/platform/src/lib.rs`, `crates/platform/src/linux.rs`, `crates/platform/src/fake.rs`

PID1 leaf-move + `subtree_control` delegation so containers can get cpu/memory/pids/io cgroups. Pure decision (`subtree_control_line`) is unit-tested; the syscall side is exercised by the boot test.

- [ ] **Step 1: Write the failing pure-function test**

Append to the `tests` module in `crates/platform/src/fake.rs` (it already has one):

```rust
    #[test]
    fn subtree_line_intersects_desired_with_available() {
        use crate::subtree_control_line;
        // All desired available → all, in desired order, each + prefixed.
        assert_eq!(subtree_control_line(&["cpu", "memory", "pids", "io"]), "+cpu +memory +pids +io");
        // io unavailable → dropped, rest kept.
        assert_eq!(subtree_control_line(&["cpu", "memory", "pids"]), "+cpu +memory +pids");
        // an unrelated available controller is ignored.
        assert_eq!(subtree_control_line(&["cpu", "rdma"]), "+cpu");
    }

    #[test]
    fn fake_records_cgroup_delegation() {
        let p = FakePlatform::new();
        p.delegate_cgroups().unwrap();
        assert!(p.recorded.lock().unwrap().cgroup_delegated);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-platform subtree_line_intersects`
Expected: FAIL (`subtree_control_line`, `delegate_cgroups`, `cgroup_delegated` undefined).

- [ ] **Step 3: Add consts + the pure helper + the trait method**

In `crates/platform/src/lib.rs`, add near the `MS_*` consts:

```rust
/// cgroup-v2 unified hierarchy mount point.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Controllers machined delegates to the root subtree so containers can get
/// cpu/memory/pids/io cgroups. Intersected with what the kernel actually offers.
pub const CGROUP_DELEGATED: &[&str] = &["cpu", "memory", "pids", "io"];
/// Leaf cgroup PID1 moves into (cgroup-v2 "no internal processes" convention).
pub const CGROUP_INIT_LEAF: &str = "init.scope";

/// The `cgroup.subtree_control` write enabling each desired controller that is
/// actually `available` (kernel `cgroup.controllers`), in `CGROUP_DELEGATED`
/// order, each `+`-prefixed and space-joined. Pure.
pub fn subtree_control_line(available: &[&str]) -> String {
    CGROUP_DELEGATED
        .iter()
        .filter(|c| available.contains(c))
        .map(|c| format!("+{c}"))
        .collect::<Vec<_>>()
        .join(" ")
}
```

Add to the `Platform` trait (after `set_hostname`):

```rust
    /// Move PID1 into a leaf cgroup and delegate controllers to the root
    /// subtree so containers get cpu/memory/pids/io cgroups (cgroup-v2). A
    /// no-op when `/sys/fs/cgroup` is not a cgroup-v2 mount.
    fn delegate_cgroups(&self) -> Result<()>;
```

- [ ] **Step 4: Implement on the Linux platform**

In `crates/platform/src/linux.rs`, add `use crate::{CGROUP_DELEGATED, CGROUP_INIT_LEAF, CGROUP_ROOT};` to the imports and the method to `impl Platform for LinuxPlatform`:

```rust
    fn delegate_cgroups(&self) -> Result<()> {
        let root = std::path::Path::new(CGROUP_ROOT);
        // cgroup.controllers exists only on a cgroup-v2 mount; absent → no-op.
        let controllers = match std::fs::read_to_string(root.join("cgroup.controllers")) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        let available: Vec<&str> = controllers.split_whitespace().collect();

        // 1. Move PID1 into the leaf so the root has no member processes before
        //    we enable subtree_control. Writing our pid to cgroup.procs moves
        //    the whole (multi-threaded) process atomically.
        let leaf = root.join(CGROUP_INIT_LEAF);
        std::fs::create_dir_all(&leaf)
            .map_err(|e| PlatformError::Other(format!("mkdir {}: {e}", leaf.display())))?;
        std::fs::write(leaf.join("cgroup.procs"), format!("{}", std::process::id()))
            .map_err(|e| PlatformError::Other(format!("move pid1 to {}: {e}", leaf.display())))?;

        // 2. Delegate the available∩desired controllers to root's children.
        let want: Vec<&str> = CGROUP_DELEGATED.iter().copied().filter(|c| available.contains(c)).collect();
        if !want.is_empty() {
            let line = crate::subtree_control_line(&available);
            std::fs::write(root.join("cgroup.subtree_control"), &line)
                .map_err(|e| PlatformError::Other(format!("subtree_control '{line}': {e}")))?;
        }
        Ok(())
    }
```

- [ ] **Step 5: Implement on the fake**

In `crates/platform/src/fake.rs`, add `pub cgroup_delegated: bool,` to `Recorded` (it derives `Default`), and the method to `impl Platform for FakePlatform`:

```rust
    fn delegate_cgroups(&self) -> Result<()> {
        self.recorded.lock().unwrap().cgroup_delegated = true;
        Ok(())
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p machined-platform`
Expected: PASS (pure helper + fake record; existing platform tests unaffected).

- [ ] **Step 7: Commit**

```bash
git add crates/platform/src
git commit -m "feat(platform): cgroup-v2 controller delegation + PID1 leaf-move"
```

---

## Task 9: overlay kernel module

**Files:**
- Modify: `crates/imager/src/modules.rs`

containerd's default overlayfs snapshotter needs `overlay.ko`. Add it to `VIRT_MODULES` (shared x86_64/aarch64; both Alpine `linux-virt` builds it `=m`).

- [ ] **Step 1: Add `overlay` to the module set**

In `crates/imager/src/modules.rs`, add `"overlay"` to `VIRT_MODULES`:

```rust
pub const VIRT_MODULES: &[&str] = &[
    "virtio_blk",
    "virtio_net",
    "ext4",
    "vfat",
    "nls_cp437",
    "nls_iso8859_1",
    "nls_utf8",
    // containerd's overlayfs snapshotter needs overlay.ko for image layers.
    "overlay",
];
```

- [ ] **Step 2: Verify `overlay` resolves in the real modules.dep**

Run: `cargo build --release -p machined-imager && cargo run --release -p machined-imager -- build --arch x86_64 --machined "$(echo /bin/true)" --config examples/node-ci.yaml --out /tmp/m8-mod.img --cache target/imager-cache 2>&1 | tail -5`
Expected: the build proceeds past module-closure resolution (no `module overlay not found in modules.dep`).

> **Branch:** if it errors `module overlay not found in modules.dep`, the Alpine kernel builds overlayfs `=y` (built-in) — overlayfs then works WITHOUT a module. In that case revert this task's edit (remove `"overlay"`) and note in the commit that overlayfs is built-in. Either outcome is correct; the Task 13 boot test is the real proof.

- [ ] **Step 3: Run the imager unit tests**

Run: `cargo test -p machined-imager modules`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/imager/src/modules.rs
git commit -m "feat(imager): load overlay.ko for containerd's overlayfs snapshotter"
```

---

## Task 10: containerd `sandbox_image` + `ctr` import argv (quarantine)

**Files:**
- Modify: `crates/config/src/runtime_svc.rs`

The two containerd-specific bits live here ONLY: the pause image the CRI uses for sandboxes, and the `ctr images import` command line. Both are pure + unit-tested.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/config/src/runtime_svc.rs`:

```rust
    #[test]
    fn config_sets_sandbox_image() {
        let toml_str = containerd_config_toml(&RuntimeSection::default());
        assert!(toml_str.contains(&format!("sandbox_image = \"{PAUSE_IMAGE}\"")), "{toml_str}");
        // still valid TOML.
        toml::from_str::<toml::Value>(&toml_str).expect("valid TOML");
    }

    #[test]
    fn ctr_import_argv_is_k8s_namespaced() {
        let argv = ctr_import_args("/run/containerd/containerd.sock", "/boot/images/busybox.tar");
        assert_eq!(
            argv,
            vec![
                "--address", "/run/containerd/containerd.sock",
                "-n", "k8s.io",
                "images", "import", "/boot/images/busybox.tar",
            ]
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-config config_sets_sandbox_image`
Expected: FAIL (`PAUSE_IMAGE`, `ctr_import_args` undefined).

- [ ] **Step 3: Add the const + the import argv + the sandbox_image line**

In `crates/config/src/runtime_svc.rs`, add near `RUNTIME_SERVICE_ID`:

```rust
/// The pre-baked pause image the containerd CRI uses for pod sandboxes. Staged
/// on /boot/images and imported at boot (see ctr_import_args). containerd-specific.
pub const PAUSE_IMAGE: &str = "registry.k8s.io/pause:3.10";

/// `ctr` argv that imports a pre-baked OCI archive into the k8s.io namespace the
/// CRI plugin reads. containerd-specific — swapping the CRI runtime swaps this.
pub fn ctr_import_args<'a>(socket: &'a str, tar: &'a str) -> Vec<&'a str> {
    vec!["--address", socket, "-n", "k8s.io", "images", "import", tar]
}
```

In `containerd_config_toml`, add the `sandbox_image` line under the CRI runtime plugin. Change the format string's plugin block opening to:

```rust
[plugins.'io.containerd.cri.v1.runtime']
  sandbox_image = "{pause}"
  [plugins.'io.containerd.cri.v1.runtime'.containerd]
```

…and add `pause = PAUSE_IMAGE` to the `format!` args alongside `socket = rt.socket`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p machined-config`
Expected: PASS (new tests + the existing `containerd_config_is_v3_cri_with_runc_cgroupfs`, which still finds all its assertions).

- [ ] **Step 5: Re-export for the daemon**

In `crates/config/src/lib.rs`, add `ctr_import_args, PAUSE_IMAGE` to the `pub use runtime_svc::{...}` list.

- [ ] **Step 6: Commit**

```bash
git add crates/config/src
git commit -m "feat(config): containerd sandbox_image + ctr image-import argv (quarantined)"
```

---

## Task 11: `oci-image` artifact kind + pre-baked images

**Files:**
- Modify: `crates/imager/src/manifest.rs`, `crates/imager/src/build.rs`, `crates/imager/artifacts.toml`
- Create: `scripts/build-oci-images.sh`

A new artifact kind stages an OCI archive to `/boot/images/<name>.tar`. The images are produced offline by the build script (digest-pinned) and hosted; their url+sha go in `artifacts.toml`.

- [ ] **Step 1: Write the failing build test**

In `crates/imager/src/build.rs` tests, extend the `fixture` to also pin one `oci-image` artifact and assert it lands on the FAT. Add to the `map`/manifest in `fixture` an entry:

```rust
        let pausetar = b"OCI-ARCHIVE-PAUSE".to_vec();
        let pausetar_sha = hex::encode(Sha256::digest(&pausetar));
        let pausetar_url = "http://example/pause.tar".to_string();
        map.insert(pausetar_url.clone(), pausetar);
```

…and append to the manifest TOML written in `fixture`:

```toml

[[artifact.x86_64]]
name = "pause-image"
url = "{pausetar_url}"
sha256 = "{pausetar_sha}"
kind = "oci-image"
rename = "pause.tar"
```

(thread `pausetar_url`/`pausetar_sha` into the `format!`). Then add an assertion to `happy_path_builds_image_with_all_boot_files`:

```rust
        // oci-image kind stages the archive under /images on the FAT.
        let images = fs.root_dir().open_dir("images").expect("images dir on FAT");
        let img_names: Vec<String> = images.iter().map(|e| e.unwrap().file_name())
            .filter(|n| n != "." && n != "..").collect();
        assert!(img_names.contains(&"pause.tar".to_string()), "{img_names:?}");
        assert_eq!(read_fat_file(&fs, "images/pause.tar"), b"OCI-ARCHIVE-PAUSE");
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-imager happy_path_builds_image_with_all_boot_files`
Expected: FAIL (`unknown artifact kind oci-image`).

- [ ] **Step 3: Document the kind in the manifest doc-comment**

In `crates/imager/src/manifest.rs`, extend the `kind` field doc-comment:

```rust
    /// "apk" → initramfs rootfs; "boot-tarball" → /boot/bin (bin/* from a
    /// single .tar.gz); "boot-binary" → /boot/bin/<rename|name>;
    /// "oci-image" → /boot/images/<rename|name> (a pre-baked OCI archive).
    pub kind: String,
```

- [ ] **Step 4: Handle the kind in the build pipeline**

In `crates/imager/src/build.rs`, add a staging-dir const near the top:

```rust
/// Subdir of the FAT staging tree where pre-baked OCI image archives land.
const IMAGES_SUBDIR: &str = "images";
```

In the fetch/extract `match a.kind.as_str()` block, add an arm before the `k =>` catch-all:

```rust
            "oci-image" => {
                let name = a.rename.clone().unwrap_or_else(|| a.name.clone());
                let dst_dir = staging.join(IMAGES_SUBDIR);
                std::fs::create_dir_all(&dst_dir)
                    .with_context(|| format!("create {}", dst_dir.display()))?;
                std::fs::copy(&path, dst_dir.join(&name))
                    .with_context(|| format!("staging oci-image {name}"))?;
            }
```

- [ ] **Step 5: Run the imager tests**

Run: `cargo test -p machined-imager`
Expected: PASS.

- [ ] **Step 6: Add the image-build script**

Create `scripts/build-oci-images.sh` (produces digest-pinned OCI archives + prints sha256 for `artifacts.toml`):

```bash
#!/usr/bin/env bash
# Produce the pre-baked OCI archives M8a stages on /boot/images, digest-pinned
# and reproducible. Requires skopeo. Run on a networked machine; upload the two
# tarballs as GHCR release assets and paste url+sha256 into artifacts.toml.
set -euo pipefail
OUT=${1:-target/oci-images}
mkdir -p "$OUT"

# Digest-pinned (amd64). Re-pin deliberately if you bump versions.
PAUSE="registry.k8s.io/pause:3.10"
BUSYBOX="docker.io/library/busybox:1.36"

emit() {  # name ref
  local name="$1" ref="$2" tar="$OUT/$1.tar"
  echo ">> $ref -> $tar"
  skopeo copy --override-arch amd64 --override-os linux \
    "docker://$ref" "oci-archive:$tar:$ref"
  echo "   sha256: $(sha256sum "$tar" | cut -d' ' -f1)"
}

emit pause   "$PAUSE"
emit busybox "$BUSYBOX"
echo "Upload $OUT/{pause,busybox}.tar and paste the url+sha256 into crates/imager/artifacts.toml"
```

Make it executable:

```bash
chmod +x scripts/build-oci-images.sh
```

- [ ] **Step 7: Pin the images in `artifacts.toml`**

> **Operator step (two-phase, like the GHCR CI image):** run `scripts/build-oci-images.sh`, upload `pause.tar` + `busybox.tar` to a GHCR release under `indyjonesnl/machined-rs`, then add to the `x86_64 = [ ... ]` list in `crates/imager/artifacts.toml` (replace `<URL>`/`<SHA>` with the real values the script prints):

```toml
  { name = "pause-image", url = "<URL>/pause.tar", sha256 = "<SHA>", kind = "oci-image", rename = "pause.tar" },
  { name = "busybox-image", url = "<URL>/busybox.tar", sha256 = "<SHA>", kind = "oci-image", rename = "busybox.tar" },
```

Then verify the manifest still parses + the x86_64 list carries the images:

Run: `cargo test -p machined-imager real_artifacts_manifest_parses`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/imager/src crates/imager/artifacts.toml scripts/build-oci-images.sh
git commit -m "feat(imager): oci-image artifact kind + pre-baked pause/busybox staging"
```

---

## Task 12: Wire it into the daemon

**Files:**
- Create: `crates/machined/src/runtime_images.rs`
- Modify: `crates/machined/src/main.rs`

Three wirings: cgroup delegation (PID1 early boot), the `ctr` image-import task (containerd-specific, spawned), and `PodController` registration. No unit tests — the boot test (Task 13) is the integration proof; each change must compile.

- [ ] **Step 1: Add the image-import task module**

Create `crates/machined/src/runtime_images.rs`:

```rust
//! Import pre-baked OCI archives from /boot/images into containerd's k8s.io
//! namespace, so the CRI sandbox + pod images are present offline. This is the
//! ONE containerd-specific runtime step in the daemon: it shells `ctr` (from
//! /boot/bin). Swapping the CRI runtime means swapping this importer.

use std::path::Path;
use std::time::Duration;

use machined_config::ctr_import_args;
use tracing::{info, warn};

const IMAGES_DIR: &str = "/boot/images";

/// Wait (bounded) for the CRI socket, then `ctr images import` every *.tar under
/// /boot/images. Best-effort: failures are logged; the PodController stays
/// Pending until the images appear, so a missed import just delays pod start.
pub async fn import_boot_images(socket: String) {
    let dir = Path::new(IMAGES_DIR);
    let tars: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "tar").unwrap_or(false))
            .collect(),
        Err(_) => return, // no /boot/images (off-image / no pods) — nothing to do.
    };
    if tars.is_empty() {
        return;
    }

    // Wait for containerd to create its socket (bounded ~60s).
    let sock = Path::new(&socket);
    for _ in 0..300 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    for tar in tars {
        let tar_s = tar.to_string_lossy().to_string();
        let args = ctr_import_args(&socket, &tar_s);
        match tokio::process::Command::new("ctr").args(&args).output().await {
            Ok(o) if o.status.success() => info!("imported image {tar_s}"),
            Ok(o) => warn!("ctr import {tar_s} failed: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => warn!("spawning ctr for {tar_s}: {e}"),
        }
    }
}
```

- [ ] **Step 2: Declare the module + import the controller**

In `crates/machined/src/main.rs`, add `mod runtime_images;` next to the other `mod` lines, and add `PodController` to the runtime-controllers import:

```rust
use machined_controllers::runtime::{PodController, RuntimeHealthController};
```

- [ ] **Step 3: Delegate cgroups in the PID1 block**

In `run_daemon`, inside `if std::process::id() == 1 { ... }` (after the `mount_boot` call), add:

```rust
        if let Err(e) = platform.delegate_cgroups() {
            error!("cgroup delegation: {e}");
        }
```

- [ ] **Step 4: Register the `PodController`**

Right after the `runtime.register(Box::new(RuntimeHealthController::new(...)))` block, add:

```rust
    runtime.register(Box::new(PodController::new(
        build_cri(&provider.runtime().socket),
        provider.clone(),
    )));
```

- [ ] **Step 5: Spawn the image-import task**

After the controller runtime is spawned (`let rt_handle = tokio::spawn(...)`), add — gated to a real image boot with the runtime enabled and pods configured, so dev/test runs never shell `ctr`:

```rust
    if std::process::id() == 1 && !provider.runtime().disabled && !provider.pods().is_empty() {
        let socket = provider.runtime().socket.clone();
        tokio::spawn(runtime_images::import_boot_images(socket));
    }
```

- [ ] **Step 6: Compile + run the workspace tests**

Run: `cargo build -p machined && cargo test --workspace`
Expected: PASS (the daemon compiles with the new wiring; all crate tests green).

- [ ] **Step 7: Commit**

```bash
git add crates/machined/src
git commit -m "feat(machined): wire cgroup delegation, image import, PodController"
```

---

## Task 13: node-ci pod + boot-test assertion

**Files:**
- Modify: `examples/node-ci.yaml`, `scripts/boot-test-x86_64.sh`

Declare the `hello` pod and assert it reaches `Running` in the x86_64 boot test.

> **Precondition:** Task 11's operator step must be done (pause + busybox hosted, `artifacts.toml` pinned), or the image build fails to fetch them.

- [ ] **Step 1: Add the pod to node-ci.yaml**

Append to `examples/node-ci.yaml` (under `machine:`):

```yaml
  pods:
    - name: hello
      image: docker.io/library/busybox:1.36
      command: ["/bin/sh", "-c"]
      args: ["echo machined-pod-ok; sleep 3600"]
      host_network: true
```

- [ ] **Step 2: Verify the example still parses**

Run: `cargo test -p machined-imager ci_example_config_parses`
Expected: PASS (the existing schema-drift guard now also covers the `pods` block).

- [ ] **Step 3: Bump the boot-test budget**

In `scripts/boot-test-x86_64.sh`, raise the overall API timeout to give image import + pod start headroom:

```bash
TIMEOUT=${BOOT_TEST_TIMEOUT:-240}
```

- [ ] **Step 4: Add the PodStatus assertion**

In `scripts/boot-test-x86_64.sh`, replace the final runtime-readiness block (the `echo "checking runtime readiness..."` loop ending in `exit 0`) so that reaching RuntimeReady proceeds to a pod check instead of passing:

```bash
echo "checking runtime readiness (namespace runtime)..."
rt_deadline=$((SECONDS + 120))
runtime_ok=0
while [ $SECONDS -lt $rt_deadline ]; do
  RT=$(ctl get RuntimeStatus --namespace runtime 2>/dev/null || true)
  if echo "$RT" | grep -Eq 'ready=true'; then
    echo "$RT"; runtime_ok=1; break
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -120 "$SERIAL"; exit 1; fi
  sleep 2
done
if [ "$runtime_ok" -ne 1 ]; then
  echo "runtime never became ready:"; ctl get RuntimeStatus --namespace runtime || true
  tail -120 "$SERIAL"; exit 1
fi

# The PodController pulls the pre-baked busybox pod up via CRI. A running pod row:
#   hello  name=hello phase=Running container_id=... message=
echo "checking pod is Running (namespace runtime)..."
pod_deadline=$((SECONDS + 180))
while [ $SECONDS -lt $pod_deadline ]; do
  PODS=$(ctl get PodStatus --namespace runtime 2>/dev/null || true)
  if echo "$PODS" | grep -Eq 'name=hello .*phase=Running'; then
    echo "$PODS"; echo "BOOT TEST PASSED"; exit 0
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -120 "$SERIAL"; exit 1; fi
  sleep 2
done
echo "pod never reached Running:"; ctl get PodStatus --namespace runtime || true
tail -160 "$SERIAL"; exit 1
```

- [ ] **Step 5: Run the boot test**

Run: `make boot-test`
Expected: serial log shows containerd RuntimeReady, the image import, and finally `name=hello phase=Running` → `BOOT TEST PASSED`. (Requires the hosted images from Task 11.)

- [ ] **Step 6: Commit**

```bash
git add examples/node-ci.yaml scripts/boot-test-x86_64.sh
git commit -m "test(boot): assert the hello pod reaches Running via CRI"
```

---

## Final verification

- [ ] **Workspace green:** `cargo fmt --all -- --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --workspace`
- [ ] **x86_64 boot test passes** (`make boot-test`) — pod Running, with the hosted images.
- [ ] **aarch64 still boots** (`make boot-test-aarch64`) — confirms adding `overlay` to the shared module set didn't regress the aarch64 RuntimeReady bar.
- [ ] **Pluggability audit:** `grep -rn "ctr\|containerd" crates/controllers/src` returns nothing (the controller is CRI-trait-only); the only `ctr`/`sandbox_image`/`PAUSE_IMAGE` references are in `crates/config/src/runtime_svc.rs` and `crates/machined/src/runtime_images.rs`.

---

## Self-Review notes (author)

- **Spec coverage:** CRI client extension (Tasks 1–4), PodController (7), pods schema (6), pre-baked images (11), cgroup delegation (8), PID1 leaf-move (8), overlay module (9), sandbox_image (10), x86 CI assertion (13). All M8a spec §1–8 items mapped. Out-of-scope §9 items (CNI, registry pull, multi-container, restart policy, aarch64 pod-run) are absent by design.
- **Type consistency:** `CriClient` methods (`image_present`/`pull_image`/`run_pod_sandbox`/`find_sandbox`/`create_container`/`start_container`/`find_container`/`container_state`) and domain types (`PodSpec`/`ContainerSpec`/`ContainerState`) are defined in Task 2–4 and used unchanged in Task 7. `PodStatus`/`PodPhase` defined in Task 5, used in 7. `subtree_control_line`/`delegate_cgroups`/`CGROUP_*` defined in Task 8. `ctr_import_args`/`PAUSE_IMAGE` defined in Task 10, used in Task 12.
- **Operator dependency** (image hosting) is explicit in Tasks 11 + 13, mirroring the accepted GHCR CI-image two-phase rollout — not a silent placeholder.
