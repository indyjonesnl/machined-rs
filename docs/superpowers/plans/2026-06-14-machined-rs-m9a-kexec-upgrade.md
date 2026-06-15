# M9a — Atomic OS Upgrade via kexec: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** machined upgrades the running OS by downloading a new image bundle, verifying its sha256, and kexec-ing into the new kernel+initramfs — STATE/PKI surviving the warm boot. The x86_64 boot test proves a v1→v2 upgrade (the image-id flips; volumes + CA persist).

**Architecture:** A new `Upgrade { url, sha256 }` action flows through the existing apiserver→channel→main-loop plumbing. The main loop gains a **prepare-then-fire** structure: download→verify→`kexec_load` run BEFORE committing to shutdown, so a bad upgrade publishes `UpgradeStatus=Failed` and the node keeps running. Only a successful kexec load proceeds to the shutdown sequence + `reboot(RB_KEXEC)`. An image-id baked into the **initramfs** (reported via the API) lets the test tell v1 from v2.

**Tech Stack:** Rust, `kexec_file_load(2)` via libc, `nix` reboot RB_KEXEC, a small blocking HTTP client (ureq) + sha2 + tar/flate2, tonic gRPC, the imager pipeline, QEMU/KVM boot test.

**Self-contained boot-test:** unlike M8, M9a's boot-test needs NO operator hosting — it builds the v2 bundle locally and serves it from the host via `python3 -m http.server`. So Task 7 runs `make boot-test` and empirically resolves the `CONFIG_KEXEC_FILE` risk.

**Reference spec:** `docs/superpowers/specs/2026-06-14-machined-rs-m9a-kexec-upgrade-design.md`.

---

## File Structure

| File | Responsibility | Tasks |
|---|---|---|
| `crates/apiserver/proto/machine.proto` | Upgrade RPC + UpgradeRequest + VersionResponse.image_id | 1 |
| `crates/apiserver/src/service.rs` | `NodeAction::Upgrade`, upgrade handler, image_id in Version | 1 |
| `crates/apiserver/src/lib.rs` | `serve_with_shutdown` gains `image_id` | 1 |
| `crates/apiserver/tests/grpc.rs` | upgrade + image_id tests | 1 |
| `crates/machinectl/src/main.rs` | `upgrade <url> <sha256>` subcommand | 1 |
| `crates/machined/src/main.rs` | read image-id; pass to serve (Task 1); the prepare-fire loop (Task 6) | 1,6 |
| `crates/resources/*` + `crates/apiserver/src/mapping.rs` | `UpgradeStatus` resource | 2 |
| `crates/platform/src/{lib,linux,fake}.rs` + `Cargo.toml` | `kexec_load` + `reboot_kexec` | 3 |
| `crates/imager/src/{build,initramfs,main}.rs` | bake `/etc/machined/image-id` + `--image-id` | 4 |
| `crates/machined/src/upgrade.rs` (new) + `Cargo.toml` | download→verify→extract→kexec_load (`prepare`) | 5 |
| `scripts/boot-test-x86_64.sh` + `Makefile` | build v2 bundle, serve, assert v1→v2 | 7 |

---

## Task 1: Upgrade API surface + image-id reporting

**Files:** `crates/apiserver/proto/machine.proto`, `crates/apiserver/src/service.rs`, `crates/apiserver/src/lib.rs`, `crates/apiserver/tests/grpc.rs`, `crates/machinectl/src/main.rs`, `crates/machined/src/main.rs`

- [ ] **Step 1: Proto.** In `crates/apiserver/proto/machine.proto`: add the RPC to `service MachineService` (after `Reset`):

```proto
  rpc Upgrade(UpgradeRequest) returns (Empty);
```

…add the message:

```proto
message UpgradeRequest { string url = 1; string sha256 = 2; }
```

…and add a field to `VersionResponse`:

```proto
message VersionResponse { string version = 1; string image_id = 2; }
```

- [ ] **Step 2: NodeAction + handler + image_id.** In `crates/apiserver/src/service.rs`:

Change `NodeAction` to carry the upgrade args (it gains a `String` so it can no longer be `Copy`):

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeAction {
    Reboot,
    Shutdown,
    Reset,
    Upgrade { url: String, sha256: String },
}
```

Add an `image_id` field to `Machine` + its constructor, and return it from `version`:

```rust
pub struct Machine {
    state: State,
    version: String,
    image_id: String,
    actions: mpsc::Sender<NodeAction>,
}

impl Machine {
    pub fn new(
        state: State,
        version: impl Into<String>,
        image_id: impl Into<String>,
        actions: mpsc::Sender<NodeAction>,
    ) -> Self {
        Self { state, version: version.into(), image_id: image_id.into(), actions }
    }
}
```

In the `version` RPC, set the field:

```rust
    async fn version(&self, _req: Request<Empty>) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: self.version.clone(),
            image_id: self.image_id.clone(),
        }))
    }
```

Import `UpgradeRequest` (add to the `use crate::pb::{...}` line) and add the handler (after `reset`):

```rust
    async fn upgrade(&self, req: Request<UpgradeRequest>) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        tracing::info!(url = %r.url, "upgrade requested via API");
        self.actions
            .send(NodeAction::Upgrade { url: r.url, sha256: r.sha256 })
            .await
            .map_err(|_| Status::unavailable("daemon is shutting down"))?;
        Ok(Response::new(Empty {}))
    }
```

- [ ] **Step 3: serve_with_shutdown gains image_id.** In `crates/apiserver/src/lib.rs`, add an `image_id: impl Into<String>` param to `serve_with_shutdown` (after `version`) and thread it into `Machine::new(...)`:

```rust
pub async fn serve_with_shutdown(
    addr: SocketAddr,
    state: State,
    version: impl Into<String>,
    image_id: impl Into<String>,
    pki: &NodePki,
    actions: tokio::sync::mpsc::Sender<NodeAction>,
    signal: impl std::future::Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    let svc = pb::machine_service_server::MachineServiceServer::new(Machine::new(
        state, version, image_id, actions,
    ));
    // ... rest unchanged
```

(Read the current body and thread `image_id` into the `Machine::new` call; the rest of the function is unchanged.)

- [ ] **Step 4: machined reads + passes image-id.** In `crates/machined/src/main.rs`, add a helper near `default_path_if_unset`:

```rust
/// The image identity baked into this initramfs by the imager (`--image-id`),
/// read from /etc/machined/image-id. Absent → "unknown". Reported via the API
/// so an operator (and the upgrade boot-test) can see which image is running.
fn read_image_id() -> String {
    std::fs::read_to_string("/etc/machined/image-id")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}
```

In `run_daemon`, find the `serve_with_shutdown(api_addr, api_state, env!("CARGO_PKG_VERSION"), &pki, ...)` call and insert the image-id (compute it once before the spawn so it's owned):

```rust
            let image_id = read_image_id();
            // ... inside the spawned task:
                machined_apiserver::serve_with_shutdown(
                    api_addr,
                    api_state,
                    env!("CARGO_PKG_VERSION"),
                    image_id,
                    &pki,
                    api_action_tx,
                    { let t = api_token; async move { t.cancelled().await } },
                )
```

(Read the exact current call + surrounding `move` closure to place `image_id` so it's moved into the task. Also: the `tokio::select!` in the main loop matches `NodeAction` — for now leave it; if the non-exhaustive `Upgrade` arm breaks compilation, add a temporary `Some(NodeAction::Upgrade { .. }) => FinalAction::Stop,` arm with a `// TODO(Task 6)` comment — Task 6 replaces the whole loop. This keeps Task 1 compiling.)

- [ ] **Step 5: machinectl subcommand.** In `crates/machinectl/src/main.rs`: add to the `Command` enum:

```rust
    /// Upgrade the node to a new image bundle (downloads, verifies, kexecs).
    Upgrade {
        /// HTTP(S) URL of the upgrade bundle (.tar.gz of vmlinuz + initramfs.img).
        url: String,
        /// Expected sha256 (hex) of the bundle.
        sha256: String,
    },
```

Add `UpgradeRequest` to the `use machined_apiserver::pb::{...}` import, and a match arm in `main`:

```rust
        Command::Upgrade { url, sha256 } => {
            client.upgrade(UpgradeRequest { url, sha256 }).await?;
            println!("upgrade requested");
        }
```

- [ ] **Step 6: Tests.** In `crates/apiserver/tests/grpc.rs`, the existing tests construct `Machine`/call `serve_with_shutdown` — update those call sites for the new `image_id` arg. Add a test that `version` returns the image_id and that `upgrade` enqueues a `NodeAction::Upgrade`. (Read the existing grpc.rs test harness; mirror the reboot/reset test for upgrade, asserting the action channel receives `NodeAction::Upgrade { url, sha256 }`.)

- [ ] **Step 7: Build + test + commit.**

```bash
cargo build --workspace && cargo test -p machined-apiserver -p machinectl && cargo test --workspace
git add crates/apiserver crates/machinectl/src crates/machined/src/main.rs
git commit -m "feat(api): Upgrade RPC + NodeAction::Upgrade + image_id in version"
```

---

## Task 2: `UpgradeStatus` resource

**Files:** Create `crates/resources/src/upgrade_status.rs`; modify `crates/resources/src/{lib,resource,metadata}.rs`, `crates/apiserver/src/mapping.rs`

Same closed-enum pattern as `PodStatus`. Touch all five sites.

- [ ] **Step 1: Create the spec.** `crates/resources/src/upgrade_status.rs`:

```rust
//! Observed progress of an in-flight OS upgrade. Pure data.

/// Phase of an upgrade machined is performing (or last attempted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradePhase {
    Downloading,
    Verifying,
    Loaded,
    Failed,
}

/// Observed state of the current/last upgrade attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeStatus {
    pub phase: UpgradePhase,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs() {
        let u = UpgradeStatus { phase: UpgradePhase::Failed, message: "sha mismatch".into() };
        assert_eq!(u.phase, UpgradePhase::Failed);
    }
}
```

- [ ] **Step 2: Wire into resources.**
- `crates/resources/src/lib.rs`: add `pub mod upgrade_status;` + `pub use upgrade_status::{UpgradePhase, UpgradeStatus};`.
- `crates/resources/src/metadata.rs`: add `UpgradeStatus` to `ResourceType` (after `PodStatus`) + the `Display` arm `ResourceType::UpgradeStatus => "UpgradeStatus",`.
- `crates/resources/src/resource.rs`: `use crate::upgrade_status::UpgradeStatus;`; add `UpgradeStatus(UpgradeStatus)` to `Resource` (after `PodStatus(PodStatus)`); add the `resource_type()` arm.

- [ ] **Step 3: Wire into mapping.** In `crates/apiserver/src/mapping.rs`: add `"UpgradeStatus" => ResourceType::UpgradeStatus,` to `parse_resource_type`, and the render arm:

```rust
        Resource::UpgradeStatus(u) => vec![
            kv("phase", format!("{:?}", u.phase)),
            kv("message", &u.message),
        ],
```

- [ ] **Step 4: Test + commit.**

```bash
cargo test -p machined-resources -p machined-apiserver && cargo build --workspace
git add crates/resources/src crates/apiserver/src/mapping.rs
git commit -m "feat(resources): UpgradeStatus resource + apiserver mapping"
```

---

## Task 3: kexec Platform primitives

**Files:** `crates/platform/Cargo.toml`, `crates/platform/src/{lib,linux,fake}.rs`

- [ ] **Step 1: Add libc dep.** In `crates/platform/Cargo.toml`, add under `[dependencies]` (check it isn't already there): `libc = "0.2"` (use the workspace pin form `libc.workspace = true` if the workspace pins libc; else a direct version). libc is needed for `SYS_kexec_file_load` + the syscall.

- [ ] **Step 2: Write the failing fake test.** Append to the `tests` module in `crates/platform/src/fake.rs`:

```rust
    #[test]
    fn fake_records_kexec_load_and_fire() {
        use std::path::Path;
        let p = FakePlatform::new();
        p.kexec_load(Path::new("/var/up/vmlinuz"), Path::new("/var/up/initramfs.img"), "console=ttyS0")
            .unwrap();
        p.reboot_kexec().unwrap();
        let rec = p.recorded.lock().unwrap();
        assert_eq!(
            rec.kexec_loaded.as_deref(),
            Some(("/var/up/vmlinuz".to_string(), "/var/up/initramfs.img".to_string(), "console=ttyS0".to_string())).as_ref()
        );
        assert!(rec.reboot_kexec);
    }
```

- [ ] **Step 3: Trait methods.** In `crates/platform/src/lib.rs`, add to the `Platform` trait (after `poweroff`):

```rust
    /// Load a new kernel+initramfs into the kexec buffer (kexec_file_load(2)).
    /// `cmdline` is used verbatim for the new kernel (typically /proc/cmdline).
    /// Must run while the files are readable (before the shutdown unmount).
    fn kexec_load(&self, kernel: &Path, initrd: &Path, cmdline: &str) -> Result<()>;
    /// Boot the previously kexec-loaded image (reboot(RB_KEXEC)). Returns only
    /// on failure (success replaces the kernel).
    fn reboot_kexec(&self) -> Result<()>;
```

- [ ] **Step 4: Linux impl.** In `crates/platform/src/linux.rs`, add the impls to `impl Platform for LinuxPlatform`:

```rust
    fn kexec_load(&self, kernel: &Path, initrd: &Path, cmdline: &str) -> Result<()> {
        use std::os::fd::AsRawFd;
        let kf = fs::File::open(kernel)
            .map_err(|e| PlatformError::Other(format!("open kernel {}: {e}", kernel.display())))?;
        let rf = fs::File::open(initrd)
            .map_err(|e| PlatformError::Other(format!("open initrd {}: {e}", initrd.display())))?;
        // cmdline must be NUL-terminated; the length passed includes the NUL.
        let c = std::ffi::CString::new(cmdline)
            .map_err(|e| PlatformError::Other(format!("cmdline: {e}")))?;
        // SAFETY: fds are valid for the duration of the call; cmdline ptr/len
        // describe a valid NUL-terminated buffer; flags=0 loads kernel+initrd.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_kexec_file_load,
                kf.as_raw_fd(),
                rf.as_raw_fd(),
                (c.as_bytes_with_nul().len()) as libc::c_ulong,
                c.as_ptr(),
                0 as libc::c_ulong,
            )
        };
        if rc != 0 {
            return Err(PlatformError::Other(format!(
                "kexec_file_load: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    fn reboot_kexec(&self) -> Result<()> {
        reboot(RebootMode::RB_KEXEC)
            .map(|_| ())
            .map_err(|e| PlatformError::Other(format!("reboot(RB_KEXEC): {e}")))
    }
```

(The `reboot`/`RebootMode` imports + `fs` are already in linux.rs. Confirm `nix::sys::reboot::RebootMode` has the `RB_KEXEC` variant — nix 0.29 does. If it does NOT, fall back to `libc::reboot(libc::LINUX_REBOOT_CMD_KEXEC)` and report the deviation.)

- [ ] **Step 5: Fake impl.** In `crates/platform/src/fake.rs`, add to `Recorded` (derives `Default`):

```rust
    pub kexec_loaded: Option<(String, String, String)>, // (kernel, initrd, cmdline)
    pub reboot_kexec: bool,
```

Add to `impl Platform for FakePlatform`:

```rust
    fn kexec_load(&self, kernel: &Path, initrd: &Path, cmdline: &str) -> Result<()> {
        self.recorded.lock().unwrap().kexec_loaded = Some((
            kernel.to_string_lossy().into_owned(),
            initrd.to_string_lossy().into_owned(),
            cmdline.to_string(),
        ));
        Ok(())
    }
    fn reboot_kexec(&self) -> Result<()> {
        self.recorded.lock().unwrap().reboot_kexec = true;
        Ok(())
    }
```

- [ ] **Step 6: Build + test + commit.**

```bash
cargo test -p machined-platform && cargo clippy -p machined-platform --all-targets -- -D warnings && cargo build --workspace --tests
cargo fmt -p machined-platform
git add crates/platform
git commit -m "feat(platform): kexec_load (kexec_file_load) + reboot_kexec primitives"
```

(`cargo build --workspace --tests` confirms no other `Platform` impl broke — there are only Linux + Fake.)

---

## Task 4: imager bakes `/etc/machined/image-id`

**Files:** `crates/imager/src/initramfs.rs`, `crates/imager/src/build.rs`, `crates/imager/src/main.rs`

- [ ] **Step 1: Add image_id to build_initramfs.** In `crates/imager/src/initramfs.rs`, add an `image_id: &str` param to `build_initramfs` (after `kver`), and write the file next to the modules.load write:

```rust
pub fn build_initramfs(
    rootfs: &Path,
    machined: &Path,
    module_paths: &[String],
    kver: &str,
    image_id: &str,
) -> anyhow::Result<Vec<u8>> {
```

…and after the `w.file("etc/machined/modules.load", ...)` line, add:

```rust
    w.file("etc/machined/image-id", 0o644, image_id.as_bytes());
```

Update every `build_initramfs(...)` call in initramfs.rs's OWN tests to pass an image_id (e.g. `"test-image"`), and add an assertion to `builds_gzip_cpio_with_init_console_and_modules_load` that the cpio contains `etc/machined/image-id`.

- [ ] **Step 2: Thread image_id through BuildOpts + build().** In `crates/imager/src/build.rs`:
- Add `pub image_id: &'a str,` to `BuildOpts`.
- At the `initramfs::build_initramfs(&rootfs, o.machined, &mods, &kver)?` call, add `o.image_id`:
  `initramfs::build_initramfs(&rootfs, o.machined, &mods, &kver, o.image_id)?`.
- Update the build.rs test `opts(...)` helper / `BuildOpts { ... }` literals to set `image_id: "test"`.

- [ ] **Step 3: Add the --image-id CLI flag.** In `crates/imager/src/main.rs`, find the `build` subcommand args (clap) and add an `--image-id` option defaulting to `"dev"`, then pass it into the `BuildOpts { ..., image_id: &image_id }`. (Read the current clap `Build` struct + the `BuildOpts` construction; mirror how `--config`/`--arch` are wired.)

- [ ] **Step 4: Build + test.**

```bash
cargo test -p machined-imager && cargo build --workspace
```
Expected: PASS. The happy-path build test now also bakes image-id (assert it lands in the initramfs if convenient; the initramfs unit test already covers the cpio entry).

- [ ] **Step 5: Commit.**

```bash
git add crates/imager/src
git commit -m "feat(imager): bake /etc/machined/image-id into the initramfs (--image-id)"
```

---

## Task 5: upgrade module — download → verify → extract → kexec_load

**Files:** Create `crates/machined/src/upgrade.rs`; modify `crates/machined/Cargo.toml`

- [ ] **Step 1: Add deps.** In `crates/machined/Cargo.toml`, add (workspace-pin form if the workspace pins them — check `sha2`, `tar`, `flate2` are workspace deps used by the imager, and reuse the same HTTP crate the imager uses if any; else add `ureq`):

```toml
ureq = { version = "2", default-features = false, features = ["tls"] }
sha2 = "0.10"
tar = "0.4"
flate2 = "1"
```

(Prefer `*.workspace = true` if the workspace `[workspace.dependencies]` already pins these — sha2/tar/flate2 are used by `crates/imager`, so pin forms likely exist; reuse them. For the HTTP client, match whatever `crates/imager` uses for fetching if it's ureq/reqwest, else ureq as above.)

- [ ] **Step 2: Write the module with pure-part tests.** Create `crates/machined/src/upgrade.rs`:

```rust
//! OS upgrade preparation: download an image bundle, verify its sha256, extract
//! the kernel+initramfs, and load them into the kexec buffer — all BEFORE the
//! daemon commits to shutting down, so a failed upgrade leaves the node running.

use std::path::{Path, PathBuf};

use machined_platform::Platform;
use machined_resources::{Resource, ResourceObject, ResourceType, UpgradePhase, UpgradeStatus};
use machined_runtime_core::{reconcile_owned, State};
use sha2::{Digest, Sha256};
use tracing::{error, info};

const NS: &str = "runtime";
const OWNER: &str = "upgrade";
const STAGE_DIR: &str = "/var/machined-upgrade";

fn publish(state: &State, phase: UpgradePhase, message: &str) {
    let obj = ResourceObject::new(
        NS,
        "upgrade",
        Resource::UpgradeStatus(UpgradeStatus { phase, message: message.to_string() }),
    );
    let _ = reconcile_owned(state, OWNER, NS, ResourceType::UpgradeStatus, vec![obj]);
}

/// Hex sha256 of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_encode(&h.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Extract `vmlinuz` + `initramfs.img` from a gzipped-tar bundle into `dir`.
/// Returns their paths. Errors if either entry is missing or escapes `dir`.
pub fn extract_bundle(tgz: &[u8], dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    let (mut kernel, mut initrd) = (None, None);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.file_name().and_then(|n| n.to_str()).map(str::to_owned);
        let Some(name) = name else { continue };
        let target = match name.as_str() {
            "vmlinuz" => dir.join("vmlinuz"),
            "initramfs.img" => dir.join("initramfs.img"),
            _ => continue,
        };
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf)?;
        std::fs::write(&target, &buf)?;
        if name == "vmlinuz" { kernel = Some(target); } else { initrd = Some(target); }
    }
    match (kernel, initrd) {
        (Some(k), Some(i)) => Ok((k, i)),
        _ => anyhow::bail!("bundle missing vmlinuz and/or initramfs.img"),
    }
}

/// Blocking HTTP GET of `url` into a byte vec (run under spawn_blocking).
fn http_get(url: &str) -> anyhow::Result<Vec<u8>> {
    let resp = ureq::get(url).call().map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf)?;
    Ok(buf)
}

/// Download + verify + extract + kexec_load. On success the new image is in the
/// kexec buffer (caller proceeds to shutdown + reboot_kexec). On ANY failure it
/// publishes UpgradeStatus=Failed and returns Err (caller keeps the node up).
pub async fn prepare(
    state: &State,
    platform: &dyn Platform,
    url: &str,
    sha256: &str,
) -> anyhow::Result<()> {
    publish(state, UpgradePhase::Downloading, url);
    let url_owned = url.to_string();
    let bytes = tokio::task::spawn_blocking(move || http_get(&url_owned)).await??;

    publish(state, UpgradePhase::Verifying, "");
    let got = sha256_hex(&bytes);
    if !got.eq_ignore_ascii_case(sha256) {
        let msg = format!("sha256 mismatch: got {got}, want {sha256}");
        publish(state, UpgradePhase::Failed, &msg);
        anyhow::bail!(msg);
    }

    let dir = Path::new(STAGE_DIR);
    let (kernel, initrd) = match extract_bundle(&bytes, dir) {
        Ok(v) => v,
        Err(e) => {
            publish(state, UpgradePhase::Failed, &e.to_string());
            return Err(e);
        }
    };

    let cmdline = platform.kernel_cmdline().unwrap_or_else(|_| "console=ttyS0".to_string());
    if let Err(e) = platform.kexec_load(&kernel, &initrd, cmdline.trim()) {
        publish(state, UpgradePhase::Failed, &e.to_string());
        error!("kexec_load failed: {e}");
        return Err(anyhow::anyhow!("kexec_load: {e}"));
    }
    info!("upgrade image loaded into kexec buffer");
    publish(state, UpgradePhase::Loaded, "");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            for (n, d) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(d.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, n, *d).unwrap();
            }
            b.finish().unwrap();
        }
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn sha256_hex_known_vector() {
        // sha256("") = e3b0c442...
        assert_eq!(&sha256_hex(b"")[..8], "e3b0c442");
    }

    #[test]
    fn extract_bundle_pulls_both_files() {
        let tgz = bundle(&[("vmlinuz", b"KERNEL"), ("initramfs.img", b"INITRD"), ("README", b"x")]);
        let dir = tempfile::tempdir().unwrap();
        let (k, i) = extract_bundle(&tgz, dir.path()).unwrap();
        assert_eq!(std::fs::read(&k).unwrap(), b"KERNEL");
        assert_eq!(std::fs::read(&i).unwrap(), b"INITRD");
    }

    #[test]
    fn extract_bundle_missing_kernel_errors() {
        let tgz = bundle(&[("initramfs.img", b"INITRD")]);
        let dir = tempfile::tempdir().unwrap();
        assert!(extract_bundle(&tgz, dir.path()).is_err());
    }

    #[test]
    fn publish_writes_upgrade_status() {
        use machined_resources::Key;
        let state = State::new();
        super::publish(&state, UpgradePhase::Failed, "sha mismatch");
        let got = state
            .get(&Key::new(NS, ResourceType::UpgradeStatus, "upgrade"))
            .unwrap();
        assert!(
            matches!(got.spec, Resource::UpgradeStatus(u) if u.phase == UpgradePhase::Failed && u.message == "sha mismatch")
        );
    }
}
```

(The download + kexec_load paths in `prepare` are integration-tested by the Task 7 boot test, not unit-tested — they need real HTTP + a real kexec. The unit tests cover the pure, deterministic parts: `sha256_hex`, `extract_bundle` happy + missing-file, and `publish`.)
```

(`hex` crate may already be a workspace dep used by the imager tests — if so, replace `hex_encode` with `hex::encode` and drop the helper. Check `crates/imager/Cargo.toml`/`build.rs` which use `hex::encode`.)

- [ ] **Step 3: Declare the module.** In `crates/machined/src/main.rs`, add `mod upgrade;` next to the other `mod` declarations. (Don't wire it into the loop yet — Task 6.)

- [ ] **Step 4: Test + commit.**

```bash
cargo test -p machined && cargo clippy -p machined --all-targets -- -D warnings && cargo build --workspace
cargo fmt -p machined
git add crates/machined/Cargo.toml crates/machined/src
git commit -m "feat(machined): upgrade prepare (download, verify, extract, kexec_load)"
```

---

## Task 6: main-loop prepare-then-fire

**Files:** `crates/machined/src/main.rs`

Wire the Upgrade action: prepare BEFORE shutdown (node stays up on failure); a successful load → `FinalAction::Kexec` → shutdown → `reboot_kexec`. No new unit tests (boot-test proves it); must compile + keep the existing daemon tests green.

- [ ] **Step 1: Add the FinalAction variant.** In the `FinalAction` enum, add `Kexec`:

```rust
enum FinalAction {
    Stop,
    Reboot,
    Poweroff,
    Reset,
    Kexec,
}
```

- [ ] **Step 2: Clone state for the upgrade loop.** Where `let state_for_reset = state.clone();` is (just before `let ctx = SequencerCtx { state, ... }`), add `let state_for_upgrade = state.clone();`.

- [ ] **Step 3: Replace the single select with a prepare-then-fire loop.** Replace the existing:

```rust
    let final_action = tokio::select! {
        _ = pid1::wait_for_termination() => FinalAction::Stop,
        a = api_action_rx.recv() => match a {
            Some(NodeAction::Reboot) => FinalAction::Reboot,
            Some(NodeAction::Shutdown) => FinalAction::Poweroff,
            Some(NodeAction::Reset) => FinalAction::Reset,
            None => FinalAction::Stop,
        },
    };
```

with a loop that lets a failed upgrade continue serving:

```rust
    let final_action = loop {
        let action = tokio::select! {
            _ = pid1::wait_for_termination() => break FinalAction::Stop,
            a = api_action_rx.recv() => a,
        };
        match action {
            Some(NodeAction::Reboot) => break FinalAction::Reboot,
            Some(NodeAction::Shutdown) => break FinalAction::Poweroff,
            Some(NodeAction::Reset) => break FinalAction::Reset,
            Some(NodeAction::Upgrade { url, sha256 }) => {
                // Prepare BEFORE committing to shutdown: a failed download /
                // verify / kexec-load leaves the node running on the current image.
                match upgrade::prepare(&state_for_upgrade, platform.as_ref(), &url, &sha256).await {
                    Ok(()) => break FinalAction::Kexec,
                    Err(e) => {
                        error!("upgrade aborted (node stays up): {e}");
                        continue;
                    }
                }
            }
            None => break FinalAction::Stop,
        }
    };
```

- [ ] **Step 4: Add the Kexec fire after the shutdown sequence.** In the final `match final_action { ... }`, add the arm (after `Reset`):

```rust
        FinalAction::Kexec => {
            info!("upgrade: booting the new image via kexec");
            if let Err(e) = platform.reboot_kexec() {
                error!("kexec reboot failed: {e}");
                park_after_failed_final(&platform, &e).await;
            }
        }
```

(The shutdown sequence already ran before this match — services stopped, `/var` unmounted. The kexec image was loaded into the kernel buffer during `prepare` while `/var` was still mounted, so the post-unmount fire is safe.)

- [ ] **Step 5: Build + test + commit.**

```bash
cargo build -p machined && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
git add crates/machined/src/main.rs
git commit -m "feat(machined): prepare-then-fire upgrade loop + kexec final action"
```

(`cargo fmt --all` + the workspace test/clippy here catch any drift from Tasks 1-5.)

---

## Task 7: boot-test proves v1 → v2

**Files:** `scripts/boot-test-x86_64.sh`, `Makefile`

This boot-test is self-contained (builds + serves the v2 bundle locally — no operator hosting). It runs `make boot-test` and empirically resolves the `CONFIG_KEXEC_FILE` risk.

- [ ] **Step 1: Build the node image as v1 + a v2 bundle.** In `scripts/boot-test-x86_64.sh`, the existing `"$IMAGER" build ... --emit-boot "$WORK/boot"` call builds the node image. Add `--image-id v1` to it. Then, after it, build a SECOND emit-boot with `--image-id v2` and tar it into a bundle:

```bash
# (existing build, now tagged v1)
"$IMAGER" build --arch x86_64 --machined "$MACHINED" \
  --config examples/node-ci.yaml --pki-dir "$WORK/pki" \
  --image-id v1 \
  --out "$IMG" --emit-boot "$WORK/boot" --cache target/imager-cache

# v2: same inputs, different image-id → a kexec target with a flipped marker.
"$IMAGER" build --arch x86_64 --machined "$MACHINED" \
  --config examples/node-ci.yaml --pki-dir "$WORK/pki" \
  --image-id v2 \
  --out "$WORK/machined-v2.img" --emit-boot "$WORK/boot-v2" --cache target/imager-cache
tar -czf "$WORK/bundle.tgz" -C "$WORK/boot-v2" vmlinuz initramfs.img
BUNDLE_SHA=$(sha256sum "$WORK/bundle.tgz" | cut -d' ' -f1)
```

- [ ] **Step 2: Serve the bundle on the host (reachable via slirp 10.0.2.2).** Before the existing QEMU launch, start a throwaway HTTP server in the work dir + ensure it's killed on exit:

```bash
UP_PORT=${UPGRADE_HTTP_PORT:-18080}
( cd "$WORK" && python3 -m http.server "$UP_PORT" --bind 0.0.0.0 >/dev/null 2>&1 ) &
HTTPD=$!
trap 'kill $QEMU $HTTPD 2>/dev/null || true; wait $QEMU $HTTPD 2>/dev/null || true' EXIT
```

(Merge this `trap` with the existing one — the existing script already traps to kill `$QEMU`; extend it to also kill `$HTTPD`. Read the current trap line and replace it.)

- [ ] **Step 3: Assert v1, trigger upgrade, assert v2 + persistence.** After the existing final assertion (currently the netpod/runtime check ending in `BOOT TEST PASSED; exit 0`), the script PASSES too early. Restructure so the last existing gate sets an `_ok` flag instead of exiting, then append the upgrade stage. Replace the final `echo "$PODS"; echo "BOOT TEST PASSED"; exit 0` (in the netpod loop) with `echo "$PODS"; pods_ok=1; break`, add a post-loop guard, then append:

```bash
# --- M9a: kexec upgrade v1 -> v2 ---
echo "asserting image-id v1..."
V1=$(ctl version 2>/dev/null || true)
echo "version: $V1"
echo "$V1" | grep -q 'image_id=v1' || { echo "expected image_id=v1, got: $V1"; tail -120 "$SERIAL"; exit 1; }

echo "triggering upgrade to v2 (http://10.0.2.2:${UP_PORT}/bundle.tgz)..."
ctl upgrade "http://10.0.2.2:${UP_PORT}/bundle.tgz" "$BUNDLE_SHA" || { echo "upgrade RPC failed"; tail -120 "$SERIAL"; exit 1; }

echo "waiting for the node to kexec into v2..."
up_deadline=$((SECONDS + 240))
while [ $SECONDS -lt $up_deadline ]; do
  V=$(ctl version 2>/dev/null || true)
  if echo "$V" | grep -q 'image_id=v2'; then
    echo "post-upgrade version: $V"
    # STATE/PKI persisted across the warm boot: the SAME machinectl bundle still
    # authenticates, and volumes are still Provisioned.
    VOLS=$(ctl get VolumeStatus --namespace block 2>/dev/null || true)
    if echo "$VOLS" | grep -Eq 'name=STATE .*phase=Provisioned' \
       && echo "$VOLS" | grep -Eq 'name=EPHEMERAL .*phase=Provisioned'; then
      echo "$VOLS"; echo "BOOT TEST PASSED (kexec upgrade v1->v2, STATE persisted)"; exit 0
    fi
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died during upgrade"; tail -200 "$SERIAL"; exit 1; fi
  sleep 2
done
echo "node never came up as v2:"; ctl version || true; ctl get UpgradeStatus --namespace runtime || true
tail -200 "$SERIAL"; exit 1
```

The `version` row is rendered by machinectl as `version=<v> image_id=<id>`? NO — `machinectl version` prints `v.version` only today. Update machinectl's `Version` arm to also print the image_id so the grep works:

In `crates/machinectl/src/main.rs`, change the `Command::Version` arm to:

```rust
        Command::Version => {
            let v = client.version(Empty {}).await?.into_inner();
            println!("version={} image_id={}", v.version, v.image_id);
        }
```

(This changes the `machinectl version` output format — update any test that asserts the old format, and note it. The boot-test greps `image_id=v1`/`image_id=v2`.)

- [ ] **Step 4: Run the boot test (empirically resolves CONFIG_KEXEC_FILE).**

Run: `make boot-test`
Expected: serial shows v1 up → upgrade requested → kexec → v2 up with STATE Provisioned → `BOOT TEST PASSED`.

> **Branch:** if the serial shows `kexec_file_load: Function not implemented` (ENOSYS) or `... Operation not permitted`, the Alpine `linux-virt` kernel lacks `CONFIG_KEXEC_FILE` (or enforces `KEXEC_SIG`). Per the spec fallback: stage the `kexec` userspace binary (add the `kexec-tools` apk to `artifacts.toml` → /boot/bin, OR an apk into the rootfs) and change `LinuxPlatform::kexec_load`/`reboot_kexec` to shell `kexec -l <kernel> --initrd=<initrd> --command-line="<cmdline>"` then `kexec -e`. Report this as a BLOCKED-needs-decision with the exact serial error before pivoting — it changes Task 3 + adds an artifact.

- [ ] **Step 5: Commit.**

```bash
git add scripts/boot-test-x86_64.sh Makefile crates/machinectl/src/main.rs
git commit -m "test(boot): assert kexec upgrade v1->v2 with STATE persistence"
```

(If `Makefile`'s `boot-test` target needs no change, drop it from the add.)

---

## Final verification

- [ ] `cargo fmt --all -- --check` (run `cargo fmt --all` first).
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `make boot-test` — the v1→v2 kexec upgrade passes (this is the milestone's proof; it also confirms `CONFIG_KEXEC_FILE`).
- [ ] **aarch64 still boots** (`make boot-test-aarch64`) — kexec deps + the build_initramfs image_id change didn't regress the RuntimeReady bar.
- [ ] **Graceful-abort sanity:** the `UpgradeStatus=Failed` path keeps the node up — covered structurally (prepare returns Err → `continue`); the boot-test exercises the happy path. (A bad-sha negative boot-test is a nice follow-up but not required.)

---

## Self-Review notes (author)

- **Spec coverage:** image-id marker (Tasks 1,4), Upgrade action + UpgradeStatus (1,2), graceful prepare-then-fire (5,6), kexec primitives (3), download+verify (5), boot-test v1→v2 (7). All spec §1-7 mapped. Out-of-scope §8 (disk persistence, A-B, rollback, signature) absent by design.
- **Type/name consistency:** `NodeAction::Upgrade { url, sha256 }` (Task 1) consumed in Task 6; `image_id` flows proto→Machine→serve→main `read_image_id` (1) ← baked by imager (4); `UpgradeStatus`/`UpgradePhase` (2) used in `upgrade::prepare` (5); `kexec_load`/`reboot_kexec` (3) called in 5/6; `FinalAction::Kexec` (6); `build_initramfs(..., image_id)` 5-arg (4); the bundle format (vmlinuz+initramfs.img tar.gz) is consistent between `extract_bundle` (5) and the boot-test tar (7).
- **Workspace-wide ripples flagged:** `NodeAction` losing `Copy` + `Machine::new`/`serve_with_shutdown` signature changes (Task 1 updates grpc.rs + main.rs); `build_initramfs` signature (Task 4 updates its tests + build.rs); `machinectl version` output format change (Task 7 updates machinectl tests). Each task runs `cargo test --workspace` or `--workspace --tests`.
- **Empirical gate:** `CONFIG_KEXEC_FILE` resolved by Task 7's real `make boot-test` (self-contained, no hosting), with a documented kexec-tools fallback.
