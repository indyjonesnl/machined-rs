# M8b — CNI Bridge Networking: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `host_network: false` pod gets a real bridge IP via CNI (veth + bridge + host-local IPAM, nftables ipMasq), with the x86_64 boot test asserting the pod reaches `Running` AND carries a non-host `10.88.x` IP.

**Architecture:** machined stays CNI-pluggable — it only stages the containernetworking plugin binaries + a swappable bridge conflist onto `/boot/cni`, points containerd at those dirs (quarantined in `runtime_svc.rs`), and loads the netfilter/bridge kernel modules. The bridge plugin does all netlink + nftables itself. The CRI client gains `PodSandboxStatus`→`pod_ip`; `PodStatus` gains a `pod_ip` field the `PodController` fills.

**Tech Stack:** Rust, tonic/prost (CRI), the imager pipeline, containerd 2.0.9 CRI CNI, containernetworking-plugins ≥v1.4 (nftables ipMasq backend), QEMU/KVM boot test.

**Pluggability guardrails (enforced in review):**
- `grep -rn "netlink\|iptables\|nftables\|nft_\|\"bridge\"\|cni0" crates/machined crates/controllers` finds nothing CNI-related (the existing `machined_netlink` node-NIC controllers are unrelated). machined writes NO pod-network netlink/iptables.
- CNI bin_dir/conf_dir live ONLY in `crates/config/src/runtime_svc.rs`. The conflist is an opaque asset machined stages, never generates from structured CNI knowledge.
- The bridge conflist is the swappable default; a different CNI = different binaries + conflist, zero machined code change.

**Reference spec:** `docs/superpowers/specs/2026-06-14-machined-rs-m8b-cni-bridge-design.md`.

**Inherited operator dependency:** M8b's green boot test still needs the M8a pause/busybox OCI images hosted + pinned (netpod reuses busybox). The CNI plugin tarball, by contrast, is a public GitHub release the implementer pins directly (no hosting).

---

## File Structure

| File | Responsibility | Tasks |
|---|---|---|
| `crates/cri/proto/runtime.proto` | add PodSandboxStatus RPC + messages | 1 |
| `crates/cri/src/{lib,grpc,fake}.rs` | `pod_ip` trait method + impls | 2 |
| `crates/cri/tests/uds.rs`, `crates/machined/tests/payload.rs` | server + FlipCri stubs for the new method | 2 |
| `crates/resources/src/pod_status.rs` + `crates/apiserver/src/mapping.rs` | `PodStatus.pod_ip` + render | 3 |
| `crates/controllers/src/runtime/pod.rs` | plumb pod_ip through status_obj + fill from CRI | 3,4 |
| `crates/imager/src/modules.rs` | veth/bridge/br_netfilter/nf_tables/nf_nat/nf_conntrack | 5 |
| `crates/imager/src/boot.rs` + `build.rs` + `manifest.rs` | `cni-plugins` artifact kind | 6 |
| `crates/imager/assets/10-machined-bridge.conflist` (new) + `build.rs` | stage the conflist to `/boot/cni/conf` | 7 |
| `crates/config/src/runtime_svc.rs` | containerd CNI bin_dir/conf_dir (quarantine) | 8 |
| `crates/imager/artifacts.toml` | pin cni-plugins tarball (x86_64) | 9 |
| `examples/node-ci.yaml` | add `netpod` + bridge-nf sysctl | 10 |
| `scripts/boot-test-x86_64.sh` | gate hello→netpod IP assertion | 11 |

No daemon (`main.rs`) wiring is needed — `netpod` is just another `pods:` entry the existing `PodController` reconciles; the plugins/conflist/modules are imager-staged + auto-loaded.

---

## Task 1: CRI proto — PodSandboxStatus

**Files:** Modify `crates/cri/proto/runtime.proto`

- [ ] **Step 1: Add the RPC.** In `service RuntimeService { ... }`, add after the `ContainerStatus` line:

```proto
  rpc PodSandboxStatus(PodSandboxStatusRequest) returns (PodSandboxStatusResponse) {}
```

- [ ] **Step 2: Add the messages.** Append (after the existing `PullImageResponse`):

```proto
message PodSandboxNetworkStatus { string ip = 1; }
message PodSandboxStatus {
  string id = 1;
  PodSandboxMetadata metadata = 2;
  PodSandboxState state = 3;
  PodSandboxNetworkStatus network = 5;
}
message PodSandboxStatusRequest { string pod_sandbox_id = 1; bool verbose = 2; }
message PodSandboxStatusResponse { PodSandboxStatus status = 1; }
```

(Field numbers from cri-api v1: PodSandboxStatus.network is 5, skipping created_at=4/linux=6 which machined doesn't read.)

- [ ] **Step 3: Verify codegen.** Run: `cargo build -p machined-cri`
Expected: PASS (the new `RuntimeServiceClient::pod_sandbox_status` method is generated).

- [ ] **Step 4: Commit.**

```bash
git add crates/cri/proto/runtime.proto
git commit -m "feat(cri): vendor PodSandboxStatus into the CRI proto"
```

---

## Task 2: CRI client — `pod_ip`

**Files:** Modify `crates/cri/src/lib.rs`, `crates/cri/src/grpc.rs`, `crates/cri/src/fake.rs`, `crates/cri/tests/uds.rs`, `crates/machined/tests/payload.rs`

Adding a trait method breaks EVERY `CriClient` impl — update grpc + fake + the FlipCri test stub, and the new server RPC breaks the uds.rs server trait. Touch all in this task.

- [ ] **Step 1: Write the failing fake test.** Append to the `tests` module in `crates/cri/src/fake.rs`:

```rust
#[tokio::test]
async fn fake_pod_ip_reflects_seeded_ip() {
    let f = FakeCriClient::new().with_version("c", "2").with_pod_ip("10.88.0.7");
    let id = f
        .run_pod_sandbox(&crate::PodSpec { name: "n".into(), uid: "u".into(), host_network: false })
        .await
        .unwrap();
    assert_eq!(f.pod_ip(&id).await.unwrap().as_deref(), Some("10.88.0.7"));
    // A fake with no seeded ip reports None.
    let g = FakeCriClient::new().with_version("c", "2");
    let gid = g
        .run_pod_sandbox(&crate::PodSpec { name: "n".into(), uid: "u".into(), host_network: false })
        .await
        .unwrap();
    assert_eq!(g.pod_ip(&gid).await.unwrap(), None);
}
```

- [ ] **Step 2: Run it to verify it fails.** Run: `cargo test -p machined-cri fake_pod_ip_reflects_seeded_ip`
Expected: FAIL (`with_pod_ip`, `pod_ip` undefined).

- [ ] **Step 3: Add the trait method.** In `crates/cri/src/lib.rs`, add to the `CriClient` trait (after `container_state`):

```rust
    /// The sandbox's assigned IP (CRI PodSandboxStatus → status.network.ip).
    /// None when the sandbox has no network status / host-network (empty ip).
    async fn pod_ip(&self, sandbox_id: &str) -> Result<Option<String>>;
```

- [ ] **Step 4: Implement on the real client.** In `crates/cri/src/grpc.rs`, add `PodSandboxStatusRequest` to the `use crate::pb::{...}` group, and add to `impl CriClient for GrpcCriClient`:

```rust
    async fn pod_ip(&self, sandbox_id: &str) -> Result<Option<String>> {
        let mut client = self.connect().await?;
        let resp = client
            .pod_sandbox_status(PodSandboxStatusRequest {
                pod_sandbox_id: sandbox_id.to_string(),
                verbose: false,
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        let ip = resp
            .status
            .and_then(|s| s.network)
            .map(|n| n.ip)
            .filter(|ip| !ip.is_empty());
        Ok(ip)
    }
```

- [ ] **Step 5: Implement on the fake.** In `crates/cri/src/fake.rs`, add to `FakeState`:

```rust
    default_sandbox_ip: Option<String>,
```

Add the builder (after `with_image`):

```rust
    /// Seed the IP every created sandbox reports via pod_ip (CNI-assigned IP sim).
    pub fn with_pod_ip(self, ip: &str) -> Self {
        self.state.lock().unwrap().default_sandbox_ip = Some(ip.to_string());
        self
    }
```

Add to `impl CriClient for FakeCriClient`:

```rust
    async fn pod_ip(&self, _sandbox_id: &str) -> Result<Option<String>> {
        Ok(self.state.lock().unwrap().default_sandbox_ip.clone())
    }
```

- [ ] **Step 6: Stub the new method in the two other `CriClient`/server impls.**

In `crates/cri/tests/uds.rs`, add `PodSandboxStatusRequest, PodSandboxStatusResponse` to the `machined_cri::pb::{...}` import and add a server-trait stub to `impl RuntimeService for FakeCriServer` (next to the other `unimplemented` stubs):

```rust
    async fn pod_sandbox_status(
        &self,
        _r: Request<PodSandboxStatusRequest>,
    ) -> Result<Response<PodSandboxStatusResponse>, Status> {
        Err(Status::unimplemented("pod_sandbox_status"))
    }
```

In `crates/machined/tests/payload.rs`, add to `impl CriClient for FlipCri` (next to its other stubs):

```rust
    async fn pod_ip(&self, _sandbox_id: &str) -> Result<Option<String>, CriError> {
        Ok(None)
    }
```

- [ ] **Step 7: Run tests + clippy + workspace.** Run:
```bash
cargo test -p machined-cri && cargo clippy -p machined-cri --all-targets -- -D warnings && cargo test --workspace
```
Expected: all PASS (the new fake test + everything else; the whole workspace, since the trait change is workspace-wide).

- [ ] **Step 8: Commit.**

```bash
git add crates/cri/src crates/cri/tests/uds.rs crates/machined/tests/payload.rs
git commit -m "feat(cri): pod_ip via PodSandboxStatus on the CriClient trait"
```

---

## Task 3: `PodStatus.pod_ip` — plumbed empty

**Files:** Modify `crates/resources/src/pod_status.rs`, `crates/apiserver/src/mapping.rs`, `crates/controllers/src/runtime/pod.rs`

Add the field everywhere and thread it through `status_obj` as a new trailing param (defaults `""` at every site). The controller fills it for real in Task 4.

- [ ] **Step 1: Add the field + update the resource test.** In `crates/resources/src/pod_status.rs`, add `pub pod_ip: String,` to `PodStatus` (after `container_id`):

```rust
pub struct PodStatus {
    pub name: String,
    pub phase: PodPhase,
    pub container_id: String,
    pub pod_ip: String,
    pub message: String,
}
```

…and add `pod_ip: String::new(),` to the `constructs_running` test's literal.

- [ ] **Step 2: Render it.** In `crates/apiserver/src/mapping.rs`, the `Resource::PodStatus(p)` arm gains a `pod_ip` field after `container_id`:

```rust
        Resource::PodStatus(p) => vec![
            kv("name", &p.name),
            kv("phase", format!("{:?}", p.phase)),
            kv("container_id", &p.container_id),
            kv("pod_ip", &p.pod_ip),
            kv("message", &p.message),
        ],
```

- [ ] **Step 3: Thread through `status_obj`.** In `crates/controllers/src/runtime/pod.rs`, change `status_obj`'s signature to add a trailing `pod_ip: &str` and set the field:

```rust
fn status_obj(
    name: &str,
    phase: PodPhase,
    container_id: &str,
    message: &str,
    pod_ip: &str,
) -> ResourceObject {
    ResourceObject::new(
        NS,
        name,
        Resource::PodStatus(PodStatus {
            name: name.to_string(),
            phase,
            container_id: container_id.to_string(),
            pod_ip: pod_ip.to_string(),
            message: message.to_string(),
        }),
    )
}
```

Append `, ""` to EVERY existing `status_obj(...)` call site (there are 8: the not-ready one in `reconcile`, and seven in `run_one` — image-absent, cri-unreachable, ensure_sandbox Err, ensure_container Err, the Running/Exited/starting/Err arms of the container_state match). Example: `status_obj(&p.name, PodPhase::Pending, "", "image not present", "")`. The Running arm stays empty-ip for now (Task 4 fills it).

- [ ] **Step 4: Build + test.** Run:
```bash
cargo test -p machined-resources -p machined-apiserver -p machined-controllers && cargo build --workspace --tests
```
Expected: PASS (every `PodStatus` literal now sets `pod_ip`; pod renders `pod_ip=`).

- [ ] **Step 5: Commit.**

```bash
git add crates/resources/src crates/apiserver/src/mapping.rs crates/controllers/src/runtime/pod.rs
git commit -m "feat(resources): PodStatus.pod_ip field, plumbed through the controller"
```

---

## Task 4: PodController fills `pod_ip` from CRI

**Files:** Modify `crates/controllers/src/runtime/pod.rs`

- [ ] **Step 1: Write the failing test.** Append to the `tests` module in `pod.rs`:

```rust
    #[tokio::test]
    async fn running_pod_reports_cni_ip() {
        let cri = Arc::new(
            FakeCriClient::new()
                .with_version("c", "2")
                .with_image("busybox:1.36")
                .with_pod_ip("10.88.0.5"),
        );
        let state = State::new();
        mark_ready(&state);
        let ctx = ReconcileCtx { state: state.clone() };
        let mut c = PodController::new(cri, provider_with_pod(false));
        c.reconcile(&ctx).await.unwrap();
        let st = pod_status(&state, "hello");
        assert_eq!(st.phase, PodPhase::Running);
        assert_eq!(st.pod_ip, "10.88.0.5");
    }
```

- [ ] **Step 2: Run it to verify it fails.** Run: `cargo test -p machined-controllers running_pod_reports_cni_ip`
Expected: FAIL (pod_ip is empty — the Running arm passes `""`).

- [ ] **Step 3: Fill the IP in the Running arm.** In `run_one`, change the container_state Running arm to fetch the sandbox IP. Replace:

```rust
            Ok(ContainerState::Running) => status_obj(&p.name, PodPhase::Running, &container, "", ""),
```

with:

```rust
            Ok(ContainerState::Running) => {
                // Best-effort: CNI may still be assigning; empty until it lands,
                // refilled on the next 5s resync.
                let ip = self.cri.pod_ip(&sandbox).await.ok().flatten().unwrap_or_default();
                status_obj(&p.name, PodPhase::Running, &container, "", &ip)
            }
```

(The `sandbox` binding from step 2 of `run_one` is in scope. Note `status_obj`'s param order is `(name, phase, container_id, message, pod_ip)` — message `""`, pod_ip `&ip`.)

- [ ] **Step 4: Run tests + clippy.** Run:
```bash
cargo test -p machined-controllers && cargo clippy -p machined-controllers --all-targets -- -D warnings
```
Expected: PASS (the new test + the existing controller tests — `runs_pod_when_ready_and_image_present` still passes because its fake has no seeded ip → pod_ip stays empty, which it doesn't assert).

- [ ] **Step 5: Commit.**

```bash
git add crates/controllers/src/runtime/pod.rs
git commit -m "feat(controllers): PodController publishes the CNI-assigned pod_ip"
```

---

## Task 5: Netfilter + bridge kernel modules

**Files:** Modify `crates/imager/src/modules.rs`, `crates/imager/src/build.rs` (test fixture)

Add the modules CNI bridge + nftables masquerade need, verified `=m` against the real kernel (the overlay precedent).

- [ ] **Step 1: Add the modules.** In `crates/imager/src/modules.rs`, append to `VIRT_MODULES` (after `"overlay"`):

```rust
    // containerd's overlayfs snapshotter needs overlay.ko for image layers.
    "overlay",
    // CNI bridge networking: veth pairs + the cni0 bridge, plus br_netfilter +
    // the nftables NAT stack the bridge plugin's ipMasq backend programs.
    "veth",
    "bridge",
    "br_netfilter",
    "nf_tables",
    "nf_nat",
    "nf_conntrack",
];
```

- [ ] **Step 2: Verify the closure resolves against the real kernel.** Run:
```bash
cargo build --release -p machined-imager
cargo run --release -p machined-imager -- build --arch x86_64 --machined /bin/true \
  --config examples/node-ci.yaml --out /tmp/m8b-mod.img --cache target/imager-cache 2>&1 | tail -20
```
Expected: the build proceeds past module-closure resolution ("image written to ..."), no `module <name> not found in modules.dep`.

> **Branch per module:** if a specific module errors `not found in modules.dep`, it is built-in (`=y`) in the Alpine linux-virt kernel — remove ONLY that name from `VIRT_MODULES` (built-in = already available, no `.ko` needed). Re-run until the build succeeds. Record which (if any) were dropped as built-in in the commit message. The other modules stay. (Expected outcome: all six are `=m`, but `nf_conntrack`/`nf_nat` are sometimes pulled built-in — verify empirically; do not guess.)

- [ ] **Step 3: Fix the build.rs test fixture.** The synthetic `MODULES_DEP`/`ALL_KO_GZ` consts in `crates/imager/src/build.rs` tests must carry every name now in `VIRT_MODULES` or `happy_path_builds_image_with_all_boot_files` fails with `module <name> not found in modules.dep`. For each module KEPT in step 2, add a dep-less line to `MODULES_DEP` and a path to `ALL_KO_GZ`:

```
kernel/drivers/net/veth.ko.gz:
kernel/net/bridge/bridge.ko.gz:
kernel/net/bridge/br_netfilter.ko.gz:
kernel/net/netfilter/nf_tables.ko.gz:
kernel/net/netfilter/nf_nat.ko.gz:
kernel/net/netfilter/nf_conntrack.ko.gz:
```

…and the matching `"kernel/drivers/net/veth.ko.gz"`, … entries in `ALL_KO_GZ`. (Add only the ones you kept. These are modelled dep-less for the fixture; real deps don't matter for the unit test — the closure resolver just needs each named root present.)

- [ ] **Step 4: Run the imager tests.** Run: `cargo test -p machined-imager`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add crates/imager/src/modules.rs crates/imager/src/build.rs
git commit -m "feat(imager): load veth/bridge/br_netfilter/nf_tables/nf_nat/nf_conntrack for CNI"
```

---

## Task 6: `cni-plugins` imager artifact kind

**Files:** Modify `crates/imager/src/boot.rs`, `crates/imager/src/build.rs`, `crates/imager/src/manifest.rs`

Extract the bridge/host-local/loopback plugin binaries from a `cni-plugins-linux-amd64-*.tgz` into `/boot/cni/bin`.

- [ ] **Step 1: Write the failing extractor test.** Append to the `tests` module in `crates/imager/src/boot.rs`:

```rust
    #[test]
    fn cni_plugins_stages_wanted_binaries_executable() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("cni.tgz");
        std::fs::write(
            &tgz,
            gzip(&tar_with(&[
                ("./bridge", b"\x7fELF-bridge"),
                ("./host-local", b"\x7fELF-hl"),
                ("./loopback", b"\x7fELF-lo"),
                ("./flannel", b"\x7fELF-fl"), // not wanted → skipped
            ])),
        )
        .unwrap();
        let cni_bin = dir.path().join("staging").join("cni").join("bin");
        extract_cni_plugins(&tgz, &cni_bin, &["bridge", "host-local", "loopback"]).unwrap();
        assert_eq!(std::fs::read(cni_bin.join("bridge")).unwrap(), b"\x7fELF-bridge");
        assert_eq!(std::fs::read(cni_bin.join("host-local")).unwrap(), b"\x7fELF-hl");
        assert_eq!(std::fs::read(cni_bin.join("loopback")).unwrap(), b"\x7fELF-lo");
        assert!(!cni_bin.join("flannel").exists(), "non-wanted plugin skipped");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(cni_bin.join("bridge")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }
```

(The `gzip`/`tar_with` helpers already exist in this test module from the boot-tarball tests.)

- [ ] **Step 2: Run it to verify it fails.** Run: `cargo test -p machined-imager cni_plugins_stages_wanted`
Expected: FAIL (`extract_cni_plugins` undefined).

- [ ] **Step 3: Add the extractor.** In `crates/imager/src/boot.rs`, add (modelled on `extract_boot_tarball`, but the CNI tarball entries are flat — `./bridge` or `bridge` — not under `bin/`, and only the `wanted` set is staged):

```rust
/// Extract the named CNI plugin binaries (flat entries like `./bridge`) from a
/// `cni-plugins-*.tgz` into `staging_cni_bin`, mode 0755. Plugins not in
/// `wanted` are skipped. Path-escape guarded like `extract_boot_tarball`.
///
/// # Errors
/// Fails on I/O errors or an entry whose path escapes containment.
pub fn extract_cni_plugins(tgz: &Path, staging_cni_bin: &Path, wanted: &[&str]) -> anyhow::Result<()> {
    let file = std::fs::File::open(tgz).with_context(|| format!("opening {}", tgz.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    std::fs::create_dir_all(staging_cni_bin)
        .with_context(|| format!("create {}", staging_cni_bin.display()))?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        guard_contained(&path)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if !wanted.contains(&name) {
            continue;
        }
        let target = staging_cni_bin.join(name);
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf)?;
        std::fs::write(&target, &buf).with_context(|| format!("write {}", target.display()))?;
        set_exec(&target)?;
    }
    Ok(())
}
```

(`guard_contained` + `set_exec` already exist in this module. Note: `guard_contained` rejects `..`/absolute; a leading `./` component is `Component::CurDir`, which `guard_contained`'s `Component::Normal`-only check would REJECT. So strip a leading `./` first: before `guard_contained`, normalize via `let path: PathBuf = path.components().filter(|c| !matches!(c, Component::CurDir)).collect();` — add `use std::path::PathBuf;` and confirm `Component` is imported. Verify the test's `./bridge` entries pass.)

- [ ] **Step 4: Document + wire the kind.** In `crates/imager/src/manifest.rs`, extend the `kind` doc-comment:

```rust
    /// "oci-image" → /boot/images/<rename|name> (a pre-baked OCI archive);
    /// "cni-plugins" → /boot/cni/bin/{bridge,host-local,loopback} (from a
    /// cni-plugins-*.tgz).
    pub kind: String,
```

In `crates/imager/src/build.rs`, add a const near `IMAGES_SUBDIR`:

```rust
/// Subdir of the FAT staging tree for CNI plugin binaries.
const CNI_BIN_SUBDIR: &str = "cni/bin";
/// The CNI plugins machined's default bridge conflist references.
const CNI_WANTED_PLUGINS: &[&str] = &["bridge", "host-local", "loopback"];
```

Add a match arm (before the `k =>` catch-all):

```rust
            "cni-plugins" => {
                let dst = staging.join(CNI_BIN_SUBDIR);
                crate::boot::extract_cni_plugins(&path, &dst, CNI_WANTED_PLUGINS)?;
            }
```

- [ ] **Step 5: Extend the build fixture to stage one cni-plugins artifact.** In `crates/imager/src/build.rs` `fixture()`, add a cni tarball blob to the `map` + a manifest entry (mirroring the oci-image fixture addition):

```rust
        let cnitgz = {
            let mut b = Vec::new();
            {
                let mut t = tar::Builder::new(&mut b);
                for (n, d) in [("./bridge", &b"ELF-br"[..]), ("./host-local", b"ELF-hl"), ("./loopback", b"ELF-lo")] {
                    let mut h = tar::Header::new_gnu();
                    h.set_size(d.len() as u64); h.set_mode(0o755); h.set_cksum();
                    t.append_data(&mut h, n, d).unwrap();
                }
                t.finish().unwrap();
            }
            gzip(&b)
        };
        let cnitgz_sha = hex::encode(Sha256::digest(&cnitgz));
        let cnitgz_url = "http://example/cni.tgz".to_string();
        map.insert(cnitgz_url.clone(), cnitgz);
```

…and append to the manifest TOML in `fixture()`:

```toml

[[artifact.x86_64]]
name = "cni-plugins"
url = "{cnitgz_url}"
sha256 = "{cnitgz_sha}"
kind = "cni-plugins"
```

(thread `cnitgz_url`/`cnitgz_sha` into the `format!`). Add an assertion to `happy_path_builds_image_with_all_boot_files`:

```rust
        // cni-plugins kind stages the wanted plugins under /cni/bin on the FAT.
        let cni = fs.root_dir().open_dir("cni").unwrap().open_dir("bin").expect("cni/bin on FAT");
        let cni_names: Vec<String> = cni.iter().map(|e| e.unwrap().file_name())
            .filter(|n| n != "." && n != "..").collect();
        for want in ["bridge", "host-local", "loopback"] {
            assert!(cni_names.contains(&want.to_string()), "{cni_names:?}");
        }
        assert_eq!(read_fat_file(&fs, "cni/bin/bridge"), b"ELF-br");
```

- [ ] **Step 6: Run the imager tests.** Run: `cargo test -p machined-imager`
Expected: PASS.

- [ ] **Step 7: Commit.**

```bash
git add crates/imager/src
git commit -m "feat(imager): cni-plugins artifact kind → /boot/cni/bin"
```

---

## Task 7: Stage the bridge conflist

**Files:** Create `crates/imager/assets/10-machined-bridge.conflist`; modify `crates/imager/src/build.rs`

The conflist is a committed, embedded asset the imager writes to `/boot/cni/conf` — opaque to machined (swappable default).

- [ ] **Step 1: Create the conflist asset.** Create `crates/imager/assets/10-machined-bridge.conflist`:

```json
{
  "cniVersion": "1.0.0",
  "name": "machined-bridge",
  "plugins": [
    {
      "type": "bridge",
      "bridge": "cni0",
      "isGateway": true,
      "ipMasq": true,
      "ipMasqBackend": "nftables",
      "hairpinMode": true,
      "ipam": {
        "type": "host-local",
        "ranges": [[{ "subnet": "10.88.0.0/16" }]],
        "routes": [{ "dst": "0.0.0.0/0" }]
      }
    },
    { "type": "loopback" }
  ]
}
```

- [ ] **Step 2: Write the failing build assertion.** In `crates/imager/src/build.rs`'s `happy_path_builds_image_with_all_boot_files`, add (after the cni/bin assertions from Task 6):

```rust
        // The bridge conflist is staged to /cni/conf on the FAT.
        let conf = read_fat_file(&fs, "cni/conf/10-machined-bridge.conflist");
        let conf_s = String::from_utf8_lossy(&conf);
        assert!(conf_s.contains("\"type\": \"bridge\""), "{conf_s}");
        assert!(conf_s.contains("\"ipMasqBackend\": \"nftables\""), "{conf_s}");
```

- [ ] **Step 3: Run it to verify it fails.** Run: `cargo test -p machined-imager happy_path_builds_image_with_all_boot_files`
Expected: FAIL (no `cni/conf/...` on the FAT).

- [ ] **Step 4: Stage the conflist in the build.** In `crates/imager/src/build.rs`, add a const near `CNI_BIN_SUBDIR`:

```rust
/// Subdir of the FAT staging tree for the CNI conflist.
const CNI_CONF_SUBDIR: &str = "cni/conf";
/// The default bridge conflist, embedded at build time (swappable default).
const BRIDGE_CONFLIST: &str = include_str!("../assets/10-machined-bridge.conflist");
```

In `build()`, AFTER the fetch/extract loop and near where other staging files (config.yaml etc.) are written, always write the conflist (the CNI bin dir may be empty off-this-config, but the conflist is harmless to stage; keep it unconditional for simplicity):

```rust
    let cni_conf_dir = staging.join(CNI_CONF_SUBDIR);
    std::fs::create_dir_all(&cni_conf_dir)
        .with_context(|| format!("create {}", cni_conf_dir.display()))?;
    std::fs::write(cni_conf_dir.join("10-machined-bridge.conflist"), BRIDGE_CONFLIST)
        .with_context(|| "writing bridge conflist")?;
```

(Place this alongside the `config.yaml`/`vmlinuz` staging writes, before `image::write_image`.)

- [ ] **Step 5: Run the imager tests.** Run: `cargo test -p machined-imager`
Expected: PASS.

- [ ] **Step 6: Commit.**

```bash
git add crates/imager/assets crates/imager/src/build.rs
git commit -m "feat(imager): stage the default bridge conflist to /boot/cni/conf"
```

---

## Task 8: containerd CNI dirs (quarantine)

**Files:** Modify `crates/config/src/runtime_svc.rs`

- [ ] **Step 1: Write the failing test.** Append to the `tests` module in `crates/config/src/runtime_svc.rs`:

```rust
    #[test]
    fn config_sets_cni_dirs() {
        let toml_str = containerd_config_toml(&RuntimeSection::default());
        assert!(toml_str.contains("bin_dir = \"/boot/cni/bin\""), "{toml_str}");
        assert!(toml_str.contains("conf_dir = \"/boot/cni/conf\""), "{toml_str}");
        // still valid TOML.
        toml::from_str::<toml::Value>(&toml_str).expect("valid TOML");
    }
```

- [ ] **Step 2: Run it to verify it fails.** Run: `cargo test -p machined-config config_sets_cni_dirs`
Expected: FAIL.

- [ ] **Step 3: Add the CNI block.** In `containerd_config_toml`, append a `[...cni]` sub-table to the format string (after the runc `.options` block, before the closing `"#`):

```
      [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc.options]
        SystemdCgroup = false
        BinaryName = "/boot/bin/runc"

  [plugins.'io.containerd.cri.v1.runtime'.cni]
    bin_dir = "/boot/cni/bin"
    conf_dir = "/boot/cni/conf"
```

(No new `format!` arg — these are literal paths. Confirm no `{`/`}` introduced. The `.cni` table is a sibling sub-table of `.containerd` under the runtime plugin — valid TOML.)

- [ ] **Step 4: Run tests.** Run: `cargo test -p machined-config`
Expected: PASS (new test + the existing `containerd_config_is_v3_cri_with_runc_cgroupfs` + `config_sets_sandbox_image`, which still find their strings).

- [ ] **Step 5: Commit.**

```bash
git add crates/config/src/runtime_svc.rs
git commit -m "feat(config): point containerd CRI at the CNI bin/conf dirs (quarantined)"
```

---

## Task 9: Pin the cni-plugins tarball

**Files:** Modify `crates/imager/artifacts.toml`

- [ ] **Step 1: Resolve a real ≥v1.4 release + its sha256.** The containernetworking/plugins linux-amd64 tarball is a public GitHub release asset (same shape as the pinned containerd/runc artifacts). Pick a release ≥ v1.4.0 (e.g. v1.5.1 or the current latest). Fetch the tarball + its published `.sha256`/compute it:

```bash
ver=v1.5.1   # or the latest >= v1.4.0
url="https://github.com/containernetworking/plugins/releases/download/${ver}/cni-plugins-linux-amd64-${ver}.tgz"
curl -fsSL "$url" -o /tmp/cni.tgz
sha256sum /tmp/cni.tgz
```

(If the chosen version 404s, pick another existing release; verify the URL resolves before pinning.)

- [ ] **Step 2: Pin it in the x86_64 list.** Add to the `x86_64 = [ ... ]` list in `crates/imager/artifacts.toml` (next to the containerd/runc entries), with the REAL url + sha from step 1:

```toml
  { name = "cni-plugins", url = "<URL>", sha256 = "<SHA>", kind = "cni-plugins" },
```

- [ ] **Step 3: Verify the manifest parses + extend the manifest-shape test.** In `crates/imager/src/manifest.rs`, the `real_artifacts_manifest_parses` test asserts x86_64 carries the runtime artifacts; add an assertion that it now also carries the cni-plugins entry:

```rust
        assert!(x86.iter().any(|a| a.name == "cni-plugins" && a.kind == "cni-plugins"));
```

Run: `cargo test -p machined-imager real_artifacts_manifest_parses`
Expected: PASS.

- [ ] **Step 4: Commit.**

```bash
git add crates/imager/artifacts.toml crates/imager/src/manifest.rs
git commit -m "feat(imager): pin containernetworking cni-plugins (amd64, nftables ipMasq)"
```

---

## Task 10: node-ci `netpod` + bridge-nf sysctl

**Files:** Modify `examples/node-ci.yaml`

- [ ] **Step 1: Add the sysctl + the CNI pod.** In `examples/node-ci.yaml`, add a `sysctls:` block under `machine:` (the schema already supports `sysctls: [{key, value}]`) and a second pod under `pods:`:

```yaml
  sysctls:
    - key: net.bridge.bridge-nf-call-iptables
      value: "1"
  runtime:
    disabled: false
    binary: /boot/bin/containerd
  pods:
    - name: hello
      image: docker.io/library/busybox:1.36
      command: ["/bin/sh", "-c"]
      args: ["echo machined-pod-ok; sleep 3600"]
      host_network: true
    - name: netpod
      image: docker.io/library/busybox:1.36
      command: ["/bin/sh", "-c"]
      args: ["echo machined-netpod-ok; sleep 3600"]
      host_network: false
```

(Keep the existing `hostname`/`network`/`install`/`runtime` content; `netpod` reuses the busybox image already pinned for `hello`. Place `sysctls:` where it reads naturally under `machine:` — order among machine keys doesn't matter to the parser. Match the existing 2-space indentation.)

- [ ] **Step 2: Verify it parses.** Run: `cargo test -p machined-imager ci_example_config_parses`
Expected: PASS (the schema already has `sysctls` + `pods`).

- [ ] **Step 3: Commit.**

```bash
git add examples/node-ci.yaml
git commit -m "test(node-ci): add a CNI netpod + bridge-nf-call sysctl"
```

---

## Task 11: boot-test asserts the bridge IP

**Files:** Modify `scripts/boot-test-x86_64.sh`

- [ ] **Step 1: Gate hello→netpod.** In `scripts/boot-test-x86_64.sh`, the final block currently passes when `hello` is Running. Replace the `hello`-Running success (`echo "$PODS"; echo "BOOT TEST PASSED"; exit 0`) so reaching `hello=Running` no longer exits — it breaks into a netpod check. Replace the whole pod block (from `echo "checking pod is Running..."` through its final `exit 1`) with:

```bash
echo "checking host-net pod is Running (namespace runtime)..."
pod_deadline=$((SECONDS + 180))
hello_ok=0
while [ $SECONDS -lt $pod_deadline ]; do
  PODS=$(ctl get PodStatus --namespace runtime 2>/dev/null || true)
  if echo "$PODS" | grep -Eq 'name=hello .*phase=Running'; then
    echo "$PODS"; hello_ok=1; break
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -120 "$SERIAL"; exit 1; fi
  sleep 2
done
if [ "$hello_ok" -ne 1 ]; then
  echo "hello pod never reached Running:"; ctl get PodStatus --namespace runtime || true
  tail -160 "$SERIAL"; exit 1
fi

# netpod is host_network:false → CNI bridge assigns it a 10.88.x address.
# A running CNI pod row: netpod  name=netpod phase=Running container_id=... pod_ip=10.88.0.x message=
echo "checking CNI pod has a bridge IP (namespace runtime)..."
net_deadline=$((SECONDS + 180))
while [ $SECONDS -lt $net_deadline ]; do
  PODS=$(ctl get PodStatus --namespace runtime 2>/dev/null || true)
  if echo "$PODS" | grep -Eq 'name=netpod .*phase=Running .*pod_ip=10\.88\.'; then
    echo "$PODS"; echo "BOOT TEST PASSED"; exit 0
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -120 "$SERIAL"; exit 1; fi
  sleep 2
done
echo "netpod never got a bridge IP:"; ctl get PodStatus --namespace runtime || true
tail -200 "$SERIAL"; exit 1
```

(The grep `name=netpod .*phase=Running .*pod_ip=10\.88\.` matches the row's ordered fields name→phase→…→pod_ip; `10\.88\.` is the conflist subnet and is `≠ 10.0.2.15` by construction. Keep the existing `ctl()`/`$QEMU`/`$SERIAL` conventions.)

- [ ] **Step 2: Static verification.** Run:
```bash
bash -n scripts/boot-test-x86_64.sh
```
Expected: no syntax errors. (Do NOT run `make boot-test` — it needs the M8a OCI images hosted; that's the inherited operator step.)

- [ ] **Step 3: Commit.**

```bash
git add scripts/boot-test-x86_64.sh
git commit -m "test(boot): assert netpod gets a non-host bridge IP via CNI"
```

---

## Final verification

- [ ] `cargo fmt --all -- --check` (run `cargo fmt --all` first — task code is authored compact and rustfmt rewraps it; the CI check job runs fmt --check).
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `bash -n scripts/boot-test-x86_64.sh`
- [ ] **Pluggability audit:** `grep -rn "netlink\|iptables\|nftables\|cni0\|\"bridge\"" crates/machined crates/controllers` finds nothing CNI-related; CNI bin/conf dirs appear ONLY in `crates/config/src/runtime_svc.rs`; the conflist exists only as the imager asset.
- [ ] **Operator handoff:** the boot test goes green once the M8a pause/busybox OCI images are hosted + pinned in `artifacts.toml` (inherited from M8a). The cni-plugins tarball is already pinned (public release). Boot-test verification (`make boot-test`) is deferred to that hosting.

---

## Self-Review notes (author)

- **Spec coverage:** PodSandboxStatus/pod_ip (Tasks 1-2,4), PodStatus.pod_ip (3), modules (5), cni-plugins kind (6), conflist (7), containerd CNI dirs (8), pinned plugins (9), netpod + sysctl (10), boot-test IP assertion (11). All spec §1-10 mapped. Out-of-scope §11 (egress assertion, pod-to-pod, portmap/firewall, aarch64 CNI) absent by design.
- **Type/name consistency:** `pod_ip` trait method (Task 2) used in Task 4; `PodStatus.pod_ip` field (Task 3) read in mapping + controller; `status_obj` 5-arg signature `(name, phase, container_id, message, pod_ip)` defined Task 3, the Running arm fills pod_ip Task 4; `extract_cni_plugins` (Task 6) + `CNI_WANTED_PLUGINS`/`CNI_BIN_SUBDIR`/`CNI_CONF_SUBDIR`/`BRIDGE_CONFLIST` consts; `cni-plugins` artifact kind string consistent across manifest/build/artifacts.
- **Workspace-wide trait change** (Task 2 `pod_ip`) explicitly updates uds.rs server + payload.rs FlipCri + runs `cargo test --workspace` — the M8a lesson applied. Final fmt sweep called out.
- **Module closure + cni-plugins version** are empirically resolved at plan-exec time (real build / real release), branch instructions included — the overlay precedent.
