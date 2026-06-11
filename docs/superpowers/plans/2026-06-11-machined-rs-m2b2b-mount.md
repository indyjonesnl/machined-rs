# machined-rs M2b-2b — Volume Mount Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M0/M1/M2a/M2b-1/M2b-2a merged to `main`. Work on branch `spec/machined-rs-m2b2b-mount`.

**Goal:** Mount the provisioned system volumes at fixed mountpoints (EFI→`/boot`, STATE→`/system/state`, EPHEMERAL→`/var`), completing the block pipeline discover→provision→mount. Idempotent (skip already-mounted), non-destructive, mount-only.

**Architecture:** Extend the `Platform` trait with `is_mounted` (idempotency check; `LinuxPlatform` reads `/proc/self/mountinfo`, `FakePlatform` checks recorded mounts). A `VolumeMountController` consumes `VolumeStatus` (from M2b-2a), maps each system volume's label to its mountpoint, mounts it iff not already mounted, and publishes `MountStatus` via `reconcile_owned`.

**Tech Stack:** Builds on the existing stack; `platform::mount` (mount(2) via nix) already works for real filesystems and creates the target dir.

---

## File Structure

```
crates/platform/src/lib.rs           # MODIFY: Platform::is_mounted (trait method)
crates/platform/src/linux.rs         # MODIFY: LinuxPlatform::is_mounted (/proc) + unit test
crates/platform/src/fake.rs          # MODIFY: FakePlatform::is_mounted + test
crates/platform/tests/loopback_mount.rs   # NEW: gated loopback mount integration test
crates/resources/src/block.rs        # MODIFY: MountStatus
crates/resources/src/{metadata,resource,lib}.rs   # MODIFY: 1 variant + re-export
crates/controllers/src/block/mount.rs     # NEW: mountpoint map + VolumeMountController
crates/controllers/src/block/mod.rs  # MODIFY: pub mod mount + re-export
crates/machined/src/main.rs          # MODIFY: register VolumeMountController
crates/machined/tests/mount.rs       # NEW: full-pipeline e2e (discover→provision→mount)
```

---

## Task 1: `Platform::is_mounted`

**Files:**
- Modify: `crates/platform/src/lib.rs`
- Modify: `crates/platform/src/linux.rs`
- Modify: `crates/platform/src/fake.rs`
- Create: `crates/platform/tests/loopback_mount.rs`

- [ ] **Step 1: Add the trait method**

In `crates/platform/src/lib.rs`, add to the `Platform` trait (after `kernel_cmdline`):

```rust
    /// Whether something is currently mounted at `target`.
    fn is_mounted(&self, target: &str) -> Result<bool>;
```

- [ ] **Step 2: Implement for FakePlatform with a test**

In `crates/platform/src/fake.rs`, add to the `Platform` impl:

```rust
    fn is_mounted(&self, target: &str) -> Result<bool> {
        Ok(self
            .recorded
            .lock()
            .unwrap()
            .mounts
            .iter()
            .any(|m| m.target == target))
    }
```

Add a test to the `tests` module in `fake.rs`:

```rust
    #[test]
    fn fake_tracks_is_mounted() {
        let p = FakePlatform::new();
        p.mount(&MountSpec {
            source: "/dev/sda2".into(),
            target: "/var".into(),
            fstype: "ext4".into(),
            flags: 0,
            data: None,
        })
        .unwrap();
        assert!(p.is_mounted("/var").unwrap());
        assert!(!p.is_mounted("/boot").unwrap());
    }
```

- [ ] **Step 3: Implement for LinuxPlatform with a test**

In `crates/platform/src/linux.rs`, add to the `Platform` impl:

```rust
    fn is_mounted(&self, target: &str) -> Result<bool> {
        // /proc/self/mountinfo field 5 (1-based) is the mount point.
        let content = std::fs::read_to_string("/proc/self/mountinfo")?;
        Ok(content
            .lines()
            .any(|line| line.split_whitespace().nth(4) == Some(target)))
    }
```

Add a test module at the end of `linux.rs` (reading `/proc` is unprivileged):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_mounted_bogus_is_not() {
        let p = LinuxPlatform::new();
        assert!(p.is_mounted("/").unwrap(), "/ must be mounted");
        assert!(!p.is_mounted("/no/such/mountpoint").unwrap());
    }
}
```

- [ ] **Step 4: Run unit tests + clippy**

Run: `cargo test -p machined-platform`
Expected: PASS — `fake_records_mounts_and_sysctls`, `fake_tracks_is_mounted`, `root_is_mounted_bogus_is_not`.

Run: `cargo clippy -p machined-platform --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Add the gated loopback mount integration test**

Create `crates/platform/tests/loopback_mount.rs`:

```rust
//! Privileged integration test: mount a real loop-backed ext4 filesystem via
//! LinuxPlatform and confirm is_mounted. Ignored by default; run with:
//!   sudo -E cargo test -p machined-platform --test loopback_mount -- --ignored
//! Requires root (losetup + mkfs.ext4 + mount).

#![cfg(target_os = "linux")]

use std::process::Command;

use machined_platform::{LinuxPlatform, MountSpec, Platform};

#[test]
#[ignore = "requires root (losetup + mkfs + mount)"]
fn mounts_loopback_ext4() {
    let img = std::env::temp_dir().join("mnd-mount.img");
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(32 * 1024 * 1024).unwrap();
    }
    let out = Command::new("losetup")
        .args(["-f", "--show", img.to_str().unwrap()])
        .output()
        .expect("losetup");
    assert!(out.status.success(), "losetup failed");
    let loopdev = String::from_utf8(out.stdout).unwrap().trim().to_string();

    assert!(Command::new("mkfs.ext4")
        .args(["-F", &loopdev])
        .status()
        .unwrap()
        .success());

    let target = std::env::temp_dir().join("mnd-mnt");
    std::fs::create_dir_all(&target).unwrap();
    let p = LinuxPlatform::new();
    p.mount(&MountSpec {
        source: loopdev.clone(),
        target: target.to_string_lossy().to_string(),
        fstype: "ext4".into(),
        flags: 0,
        data: None,
    })
    .unwrap();

    let mounted = p.is_mounted(&target.to_string_lossy()).unwrap();

    // Cleanup before asserting.
    let _ = Command::new("umount").arg(&target).status();
    let _ = Command::new("losetup").args(["-d", &loopdev]).status();
    std::fs::remove_file(&img).ok();
    std::fs::remove_dir_all(&target).ok();

    assert!(mounted, "loopback ext4 should be mounted");
}
```

- [ ] **Step 6: Confirm default run skips the loopback test + commit**

Run: `cargo test -p machined-platform` → unit tests pass; `mounts_loopback_ext4` ignored.
Run: `cargo build --workspace` → PASS (the new trait method must be implemented by every `Platform` impl — `LinuxPlatform` + `FakePlatform` both do).
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add crates/platform
git commit -m "feat(platform): is_mounted + loopback mount integration test"
```

---

## Task 2: `MountStatus` resource

**Files:**
- Modify: `crates/resources/src/block.rs`
- Modify: `crates/resources/src/metadata.rs`
- Modify: `crates/resources/src/resource.rs`
- Modify: `crates/resources/src/lib.rs`

- [ ] **Step 1: Add the ResourceType variant**

In `crates/resources/src/metadata.rs`, add `MountStatus` after `VolumeStatus` in the enum + Display:

```rust
    VolumeStatus,
    MountStatus,
}
```

```rust
            ResourceType::VolumeStatus => "VolumeStatus",
            ResourceType::MountStatus => "MountStatus",
        };
```

- [ ] **Step 2: Add the MountStatus spec + test**

Append to `crates/resources/src/block.rs`:

```rust
/// Observed state of a managed mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountStatus {
    pub volume: String,
    pub source: String,
    pub target: String,
    pub fstype: String,
    pub mounted: bool,
}
```

Add to the `tests` module in `block.rs`:

```rust
    #[test]
    fn mount_status_constructs() {
        let m = MountStatus {
            volume: "STATE".into(),
            source: "/dev/sda2".into(),
            target: "/system/state".into(),
            fstype: "ext4".into(),
            mounted: true,
        };
        assert!(m.mounted);
    }
```

- [ ] **Step 3: Add the Resource enum variant**

In `crates/resources/src/resource.rs`, extend the block import + add the variant + `resource_type` arm:

```rust
use crate::block::{DiscoveredVolume, DiskStatus, MountStatus, VolumeStatus};
```

```rust
    VolumeStatus(VolumeStatus),
    MountStatus(MountStatus),
}
```

```rust
            Resource::VolumeStatus(_) => ResourceType::VolumeStatus,
            Resource::MountStatus(_) => ResourceType::MountStatus,
        }
```

- [ ] **Step 4: Re-export**

In `crates/resources/src/lib.rs`, update the block re-export:

```rust
pub use block::{DiscoveredVolume, DiskStatus, MountStatus, VolumePhase, VolumeStatus};
```

- [ ] **Step 5: Test + clippy + commit**

Run: `cargo test -p machined-resources` → existing + `mount_status_constructs` pass.
Run: `cargo clippy -p machined-resources --all-targets -- -D warnings` → clean.
Run: `cargo build --workspace` → PASS.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/resources
git commit -m "feat(resources): MountStatus"
```

---

## Task 3: `VolumeMountController`

**Files:**
- Create: `crates/controllers/src/block/mount.rs`
- Modify: `crates/controllers/src/block/mod.rs`

- [ ] **Step 1: Write the controller + mountpoint map + tests**

Create `crates/controllers/src/block/mount.rs`:

```rust
//! Mounts provisioned system volumes at their fixed mountpoints.

use std::sync::Arc;

use async_trait::async_trait;
use machined_platform::{MountSpec, Platform};
use machined_resources::{
    MountStatus, Resource, ResourceObject, ResourceType, VolumePhase, VolumeStatus,
};
use machined_runtime_core::{
    reconcile_owned, Controller, Input, InputKind, Output, OutputKind, ReconcileCtx,
};

use super::{ctl, NS};

const OWNER: &str = "volume-mount";

/// Fixed mountpoint for each system volume label; `None` for anything else.
pub fn mountpoint(label: &str) -> Option<&'static str> {
    match label {
        "EFI" => Some("/boot"),
        "STATE" => Some("/system/state"),
        "EPHEMERAL" => Some("/var"),
        _ => None,
    }
}

pub struct VolumeMountController {
    platform: Arc<dyn Platform>,
}

impl VolumeMountController {
    pub fn new(platform: Arc<dyn Platform>) -> Self {
        Self { platform }
    }
}

#[async_trait]
impl Controller for VolumeMountController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::VolumeStatus,
            kind: InputKind::Weak,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::MountStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let mut statuses = Vec::new();
        for obj in ctx.state.list(NS, ResourceType::VolumeStatus) {
            let Resource::VolumeStatus(v) = obj.spec else {
                continue;
            };
            if v.phase != VolumePhase::Provisioned {
                continue;
            }
            let Some(target) = mountpoint(&v.label) else {
                continue;
            };

            if !self.platform.is_mounted(target).map_err(ctl)? {
                self.platform
                    .mount(&MountSpec {
                        source: v.device.clone(),
                        target: target.to_string(),
                        fstype: v.fs.clone(),
                        flags: 0,
                        data: None,
                    })
                    .map_err(ctl)?;
            }

            statuses.push(ResourceObject::new(
                NS,
                &v.label,
                Resource::MountStatus(MountStatus {
                    volume: v.label.clone(),
                    source: v.device,
                    target: target.to_string(),
                    fstype: v.fs,
                    mounted: true,
                }),
            ));
        }
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::MountStatus, statuses)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_platform::FakePlatform;
    use machined_resources::VolumeStatus;
    use machined_runtime_core::{ReconcileCtx, State};

    fn seed_volume(state: &State, label: &str, phase: VolumePhase) {
        state
            .create(ResourceObject::new(
                NS,
                label,
                Resource::VolumeStatus(VolumeStatus {
                    name: label.into(),
                    device: format!("/dev/sda-{label}"),
                    fs: "ext4".into(),
                    label: label.into(),
                    phase,
                }),
            ))
            .unwrap();
    }

    #[test]
    fn mountpoint_map() {
        assert_eq!(mountpoint("EFI"), Some("/boot"));
        assert_eq!(mountpoint("STATE"), Some("/system/state"));
        assert_eq!(mountpoint("EPHEMERAL"), Some("/var"));
        assert_eq!(mountpoint("DATA"), None);
    }

    #[tokio::test]
    async fn mounts_provisioned_volumes_idempotently() {
        let platform = Arc::new(FakePlatform::new());
        let state = State::new();
        seed_volume(&state, "EFI", VolumePhase::Provisioned);
        seed_volume(&state, "STATE", VolumePhase::Provisioned);
        seed_volume(&state, "EPHEMERAL", VolumePhase::Provisioned);
        // A non-system volume that must be ignored.
        seed_volume(&state, "DATA", VolumePhase::Provisioned);

        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeMountController::new(platform.clone());
        c.reconcile(&ctx).await.unwrap();

        // Three system volumes mounted; DATA ignored.
        assert_eq!(platform.recorded.lock().unwrap().mounts.len(), 3);
        assert_eq!(state.list(NS, ResourceType::MountStatus).len(), 3);

        // Second reconcile: all already mounted → no new mount calls.
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(platform.recorded.lock().unwrap().mounts.len(), 3);
    }

    #[tokio::test]
    async fn skips_unprovisioned() {
        let platform = Arc::new(FakePlatform::new());
        let state = State::new();
        seed_volume(&state, "STATE", VolumePhase::Failed);
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeMountController::new(platform.clone());
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(platform.recorded.lock().unwrap().mounts.len(), 0);
        assert_eq!(state.list(NS, ResourceType::MountStatus).len(), 0);
    }
}
```

> The test reads `platform.recorded` directly — `FakePlatform.recorded` is `pub` (used the same way
> in the sequencer tests), so this works without new accessors.

- [ ] **Step 2: Wire the module**

In `crates/controllers/src/block/mod.rs`, add:

```rust
pub mod discovery;
pub mod mount;
pub mod provision;

pub use discovery::DiskDiscoveryController;
pub use mount::{mountpoint, VolumeMountController};
pub use provision::{plan_provisioning, ProvisionDecision, VolumeProvisionerController};
```

- [ ] **Step 3: Test + clippy + commit**

Run: `cargo test -p machined-controllers mount` → `mountpoint_map`, `mounts_provisioned_volumes_idempotently`, `skips_unprovisioned` pass.
Run: `cargo test -p machined-controllers` → all controller tests pass.
Run: `cargo clippy -p machined-controllers --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/controllers
git commit -m "feat(controllers): VolumeMountController (mount provisioned volumes)"
```

---

## Task 4: machined wiring + full-pipeline e2e

**Files:**
- Modify: `crates/machined/src/main.rs`
- Create: `crates/machined/tests/mount.rs`

- [ ] **Step 1: Register the mount controller**

In `crates/machined/src/main.rs`, update the controllers import:

```rust
use machined_controllers::block::{
    DiskDiscoveryController, VolumeMountController, VolumeProvisionerController,
};
```

In `run_daemon`, after the `VolumeProvisionerController` registration (and before the runtime spawn), add:

```rust
    runtime.register(Box::new(VolumeMountController::new(platform.clone())));
```

> `platform` is the `Arc<dyn Platform>` already built earlier in `run_daemon` (it derives no special
> traits beyond `Platform`; it is `Arc`-cloned for the sequencer and now the mount controller).

- [ ] **Step 2: Build + smoke test**

Run: `cargo build --workspace` → PASS.
Run: `cargo run -p machined -- version` → `machined 0.1.0`.

- [ ] **Step 3: Write the full-pipeline e2e**

Create `crates/machined/tests/mount.rs`:

```rust
//! End-to-end: the full block pipeline on the real Runtime against fakes —
//! discovery → provision (wipe:true) → mount. Asserts the provisioned volumes
//! are mounted at their fixed mountpoints. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_block::{DiskInfo, FakeBlockBackend};
use machined_config::{InstallSection, MachineConfig, MachineSection, Provider};
use machined_controllers::block::{
    DiskDiscoveryController, VolumeMountController, VolumeProvisionerController, NS,
};
use machined_platform::FakePlatform;
use machined_resources::ResourceType;
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn pipeline_discovers_provisions_and_mounts() {
    let block = Arc::new(FakeBlockBackend::new().with_disk(DiskInfo {
        name: "vda".into(),
        path: "/dev/vda".into(),
        size_bytes: 16 << 30,
        model: "VIRT".into(),
        serial: "V1".into(),
        rotational: false,
        read_only: false,
    }));
    let platform = Arc::new(FakePlatform::new());

    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: Default::default(),
            install: Some(InstallSection {
                disk: "/dev/vda".into(),
                wipe: true,
            }),
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(DiskDiscoveryController::new(block.clone())));
    runtime.register(Box::new(VolumeProvisionerController::new(
        block.clone(),
        Provider::new(config),
    )));
    runtime.register(Box::new(VolumeMountController::new(platform.clone())));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut ok = false;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if state.list(NS, ResourceType::MountStatus).len() == 3 {
            ok = true;
            break;
        }
    }
    assert!(ok, "pipeline did not mount the 3 provisioned volumes");

    // The three system volumes are mounted at their fixed targets.
    let targets: Vec<String> = platform
        .recorded
        .lock()
        .unwrap()
        .mounts
        .iter()
        .map(|m| m.target.clone())
        .collect();
    assert!(targets.contains(&"/boot".to_string()));
    assert!(targets.contains(&"/system/state".to_string()));
    assert!(targets.contains(&"/var".to_string()));

    shutdown.cancel();
    let _ = handle.await;
}
```

- [ ] **Step 4: Full gate + commit**

Run: `cargo test -p machined --test mount` → PASS.
Run: `make pre-commit` → fmt + clippy -D warnings + full workspace test all green (loopback/netns ignored).

```bash
git add crates/machined
git commit -m "feat(machined): register VolumeMountController + full-pipeline e2e"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** `Platform::is_mounted` (Linux /proc + fake) + gated loopback mount test (Task 1) ✓; `MountStatus` resource (Task 2) ✓; `mountpoint` map + `VolumeMountController` mounting provisioned system volumes idempotently via `reconcile_owned` (Task 3) ✓; machined wiring + full-pipeline e2e (Task 4) ✓.
- **Deliberate M2b-2b limits (per spec):** mount-only (no unmount — M5); fixed system layout only; no bind/overlay/options; EFI mounted but no bootloader content. No discovery-style barrier needed (empty `VolumeStatus` = nothing to mount = harmless).
- **Idempotency:** `is_mounted` is checked before `mount`; the controller test asserts a second reconcile issues **zero** new `mount` calls. Per-reconcile the controller publishes `MountStatus(mounted: true)` for each system volume via `reconcile_owned` (so removed volumes GC their `MountStatus`).
- **Type consistency:** `Platform::is_mounted`/`MountSpec` (platform) ↔ `MountStatus`/`VolumeStatus`/`VolumePhase` (resources); `reconcile_owned`/`Controller`/`Input(Weak)`/`Output(Exclusive)` (runtime-core). The controller reads `VolumeStatus` from the store (published by M2b-2a's provisioner) — in machined the provisioner is registered before the mount controller.
- **Placeholder scan:** none; every step ships complete code + exact commands.
