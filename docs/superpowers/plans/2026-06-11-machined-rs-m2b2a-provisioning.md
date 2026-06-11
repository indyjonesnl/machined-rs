# machined-rs M2b-2a — Block Provisioning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M0/M1/M2a/M2b-1 merged to `main`. Work on branch `spec/machined-rs-m2b2a-provisioning`.

**Goal:** Provision the install disk — lay out a fixed GPT (EFI/STATE/EPHEMERAL) and create filesystems — **guarded** by a pure `plan_provisioning` decision so it only ever touches the config-named `install.disk` and never wipes foreign data without `install.wipe:true`.

**Architecture:** Provisioning lives on a `BlockProvisioner: BlockBackend` supertrait (so discovery-only backends don't carry destructive methods). The safety decision is a pure function classifying the disk's current state (from M2b-1 discovery) into `Skip`/`Provision`/`RefuseForeign`; the controller executes the destructive `wipe`/`create_partitions`/`format` only inside the `Provision` branch. `FakeBlockBackend` simulates provisioning in memory (so the controller's decide→act→verify loop is fully unit-tested); `SysfsBlock` does it for real (gpt-crate write + kernel re-read + `mkfs` shell-out), covered by a gated loopback test.

**Tech Stack:** `gpt` (write), `tokio::process` (`mkfs.ext4`/`mkfs.vfat`), `nix` (BLKRRPART ioctl), plus the existing stack.

---

## File Structure

```
crates/resources/src/block.rs        # MODIFY: VolumeStatus + VolumePhase
crates/resources/src/{metadata,resource,lib}.rs   # MODIFY: 1 variant + re-exports
crates/config/src/{types,provider,lib}.rs         # MODIFY: InstallSection + install()
crates/block/src/lib.rs              # MODIFY: PartType, PartitionPlan, BlockProvisioner trait
crates/block/src/fake.rs             # MODIFY: interior mutability + BlockProvisioner sim + call recording
crates/block/src/sysfs.rs            # MODIFY: impl BlockProvisioner (spike)
crates/block/tests/loopback.rs       # MODIFY: add provisioning round-trip (gated)
crates/controllers/src/block/provision.rs   # NEW: plan_provisioning + fixed_layout + VolumeProvisionerController
crates/controllers/src/block/mod.rs  # MODIFY: re-export
crates/machined/src/main.rs          # MODIFY: build provisioner backend + register controller
crates/machined/tests/provision.rs   # NEW: e2e provisioning against fake
```

---

## Task 1: `VolumeStatus` resource

**Files:**
- Modify: `crates/resources/src/block.rs`
- Modify: `crates/resources/src/metadata.rs`
- Modify: `crates/resources/src/resource.rs`
- Modify: `crates/resources/src/lib.rs`

- [ ] **Step 1: Add the ResourceType variant**

In `crates/resources/src/metadata.rs`, add `VolumeStatus` after `DiscoveredVolume` in the enum and the `Display` match:

```rust
    DiscoveredVolume,
    VolumeStatus,
}
```

```rust
            ResourceType::DiscoveredVolume => "DiscoveredVolume",
            ResourceType::VolumeStatus => "VolumeStatus",
        };
```

- [ ] **Step 2: Add the VolumeStatus spec + VolumePhase**

Append to `crates/resources/src/block.rs`:

```rust
/// Lifecycle phase of a managed volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumePhase {
    Provisioned,
    Failed,
}

/// A managed volume the provisioner owns (EFI / STATE / EPHEMERAL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeStatus {
    pub name: String,
    pub device: String,
    pub fs: String,
    pub label: String,
    pub phase: VolumePhase,
}
```

Add a test to the existing `tests` module in `block.rs`:

```rust
    #[test]
    fn volume_status_constructs() {
        let v = VolumeStatus {
            name: "STATE".into(),
            device: "/dev/sda2".into(),
            fs: "ext4".into(),
            label: "STATE".into(),
            phase: VolumePhase::Provisioned,
        };
        assert_eq!(v.phase, VolumePhase::Provisioned);
    }
```

- [ ] **Step 3: Add the Resource enum variant**

In `crates/resources/src/resource.rs`, extend the block import and add the variant + `resource_type` arm:

```rust
use crate::block::{DiscoveredVolume, DiskStatus, VolumeStatus};
```

```rust
    DiscoveredVolume(DiscoveredVolume),
    VolumeStatus(VolumeStatus),
}
```

```rust
            Resource::DiscoveredVolume(_) => ResourceType::DiscoveredVolume,
            Resource::VolumeStatus(_) => ResourceType::VolumeStatus,
        }
```

- [ ] **Step 4: Re-export**

In `crates/resources/src/lib.rs`, update the block re-export:

```rust
pub use block::{DiscoveredVolume, DiskStatus, VolumePhase, VolumeStatus};
```

- [ ] **Step 5: Test + clippy + commit**

Run: `cargo test -p machined-resources` → existing + `volume_status_constructs` pass.
Run: `cargo clippy -p machined-resources --all-targets -- -D warnings` → clean.
Run: `cargo build --workspace` → PASS (guard against non-exhaustive matches in other crates).
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add crates/resources
git commit -m "feat(resources): VolumeStatus + VolumePhase"
```

---

## Task 2: `install` config section

**Files:**
- Modify: `crates/config/src/types.rs`
- Modify: `crates/config/src/provider.rs`
- Modify: `crates/config/src/lib.rs`

- [ ] **Step 1: Write the failing parse test**

Append to the `tests` module in `crates/config/src/load.rs`:

```rust
    const INSTALL_SAMPLE: &str = r#"
machine:
  install:
    disk: /dev/sda
    wipe: true
"#;

    #[test]
    fn parses_install_section() {
        let cfg = load_from_str(INSTALL_SAMPLE).unwrap();
        let install = cfg.machine.install.as_ref().unwrap();
        assert_eq!(install.disk, "/dev/sda");
        assert!(install.wipe);
    }

    #[test]
    fn install_wipe_defaults_false() {
        let cfg = load_from_str("machine:\n  install:\n    disk: /dev/vda\n").unwrap();
        assert!(!cfg.machine.install.as_ref().unwrap().wipe);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p machined-config parses_install_section`
Expected: FAIL — `install` field/type missing (compile error).

- [ ] **Step 3: Add the InstallSection type + field**

In `crates/config/src/types.rs`, add a `#[serde(default)] install` field to `MachineSection` (after `network`):

```rust
    /// Disk installation target + wipe policy.
    #[serde(default)]
    pub install: Option<InstallSection>,
```

Append the type:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallSection {
    /// The disk to provision, e.g. `/dev/sda`.
    pub disk: String,
    /// Wipe foreign data on the disk when provisioning. Defaults to false.
    #[serde(default)]
    pub wipe: bool,
}
```

- [ ] **Step 4: Add the Provider accessor + re-export**

In `crates/config/src/provider.rs`, add `InstallSection` to the import and a method:

```rust
use crate::types::{InstallSection, MachineConfig, NetworkSection, ServiceConfig, Sysctl};
```

```rust
    pub fn install(&self) -> Option<&InstallSection> {
        self.config.machine.install.as_ref()
    }
```

In `crates/config/src/lib.rs`, add `InstallSection` to the `types` re-export list:

```rust
pub use types::{
    InstallSection, InterfaceConfig, MachineConfig, MachineSection, NetworkSection, RestartPolicy,
    RouteConfig, ServiceConfig, Sysctl,
};
```

- [ ] **Step 4b: Update existing `MachineSection { ... }` literals (the new field breaks them)**

Adding the `install` field makes every explicit `MachineSection { ... }` struct literal non-exhaustive (E0063). Four test/seed sites from M1/M2a construct it fully — add `install: None,` to the `MachineSection { ... }` literal (after the `network:` field) in: `crates/sequencer/src/boot.rs`, `crates/machined/tests/boot_harness.rs`, `crates/machined/tests/network.rs`, and `crates/controllers/src/network/config_controller.rs` (the `provider()` helper). Run `cargo build --workspace` to confirm.

- [ ] **Step 5: Test + clippy + commit**

Run: `cargo test -p machined-config` → existing + 2 install tests pass.
Run: `cargo build --workspace` → PASS (the 4 literals updated above).
Run: `cargo clippy --all-targets --all-features -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/config crates/sequencer crates/controllers crates/machined
git commit -m "feat(config): install section (disk + wipe)"
```

---

## Task 3: `BlockProvisioner` trait + fake simulation

**Files:**
- Modify: `crates/block/src/lib.rs`
- Modify: `crates/block/src/fake.rs`

- [ ] **Step 1: Add PartType, PartitionPlan, and the BlockProvisioner trait**

In `crates/block/src/lib.rs`, add after the `BlockBackend` trait:

```rust
/// GPT partition type (the two types this OS lays out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartType {
    EfiSystem,
    LinuxFilesystem,
}

impl PartType {
    /// The GPT type GUID string for this partition type.
    pub fn type_guid(self) -> &'static str {
        match self {
            PartType::EfiSystem => "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
            PartType::LinuxFilesystem => "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
        }
    }
}

/// A planned partition. `size_bytes == 0` means "use the remaining space".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionPlan {
    pub label: String,
    pub part_type: PartType,
    pub fs: FsType,
    pub size_bytes: u64,
}

/// Destructive disk provisioning. A supertrait of [`BlockBackend`] so a
/// provisioner can also discover, while read-only backends need not implement
/// these. ALL three operations are idempotent from the caller's perspective:
/// re-creating the same layout / re-formatting an already-correct device
/// converges.
#[async_trait]
pub trait BlockProvisioner: BlockBackend {
    /// Destroy the partition table on `disk` (zap primary + backup GPT).
    async fn wipe(&self, disk: &str) -> Result<()>;
    /// Write a fresh GPT with `plan`; return the created partition device paths.
    async fn create_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>>;
    /// Create a filesystem on `device` with `label`.
    async fn format(&self, device: &str, fs: FsType, label: &str) -> Result<()>;
}
```

Add the re-exports (extend the existing `pub use` lines):

```rust
pub use fake::FakeBlockBackend;
pub use fsprobe::{probe_fs, FsProbe};
#[cfg(target_os = "linux")]
pub use sysfs::SysfsBlock;
```

(no change needed if `PartType`/`PartitionPlan`/`BlockProvisioner` are defined in `lib.rs` root — they're already `pub`.)

Add new `BlockError` variants for the destructive ops:

```rust
    #[error("gpt {device}: {message}")]
    Gpt { device: String, message: String },
    #[error("wipe {disk}: {message}")]
    Wipe { disk: String, message: String },
    #[error("mkfs {device}: {message}")]
    Mkfs { device: String, message: String },
}
```

- [ ] **Step 2: Convert FakeBlockBackend to interior mutability + simulate provisioning**

Replace `crates/block/src/fake.rs` with:

```rust
//! In-memory `BlockBackend` + `BlockProvisioner` for root-free tests. Simulates
//! provisioning so a controller can `provision → list_volumes` and see the
//! result, and records destructive calls so tests can assert none were made.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{
    BlockBackend, BlockProvisioner, DiskInfo, FsType, PartitionPlan, Result, VolumeInfo,
};

#[derive(Default)]
struct FakeState {
    disks: Vec<DiskInfo>,
    volumes: Vec<VolumeInfo>,
    wipes: Vec<String>,
    creates: Vec<String>,
    formats: Vec<(String, FsType, String)>,
}

#[derive(Default)]
pub struct FakeBlockBackend {
    state: Mutex<FakeState>,
}

impl FakeBlockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_disk(self, disk: DiskInfo) -> Self {
        self.state.lock().unwrap().disks.push(disk);
        self
    }

    pub fn with_volume(self, volume: VolumeInfo) -> Self {
        self.state.lock().unwrap().volumes.push(volume);
        self
    }

    /// Test inspection: disks that were wiped.
    pub fn wipes(&self) -> Vec<String> {
        self.state.lock().unwrap().wipes.clone()
    }

    /// Test inspection: disks that had partitions created.
    pub fn creates(&self) -> Vec<String> {
        self.state.lock().unwrap().creates.clone()
    }

    /// Test inspection: (device, fs, label) of each format call.
    pub fn formats(&self) -> Vec<(String, FsType, String)> {
        self.state.lock().unwrap().formats.clone()
    }
}

/// The bare device name from a path or name (`/dev/sda` -> `sda`).
fn disk_leaf(disk: &str) -> String {
    disk.rsplit('/').next().unwrap_or(disk).to_string()
}

fn part_device(disk: &str, num: usize) -> String {
    let sep = if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    // disk here is a /dev path; keep the path prefix.
    format!("{disk}{sep}{num}")
}

#[async_trait]
impl BlockBackend for FakeBlockBackend {
    async fn list_disks(&self) -> Result<Vec<DiskInfo>> {
        Ok(self.state.lock().unwrap().disks.clone())
    }
    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>> {
        Ok(self.state.lock().unwrap().volumes.clone())
    }
}

#[async_trait]
impl BlockProvisioner for FakeBlockBackend {
    async fn wipe(&self, disk: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.wipes.push(disk.to_string());
        let leaf = disk_leaf(disk);
        st.volumes.retain(|v| v.disk != leaf);
        Ok(())
    }

    async fn create_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>> {
        let mut st = self.state.lock().unwrap();
        st.creates.push(disk.to_string());
        // Mirror SysfsBlock: VolumeInfo.disk is the bare device name (e.g. "sda"),
        // while device is the full /dev path.
        let leaf = disk_leaf(disk);
        let mut devices = Vec::new();
        for (i, p) in plan.iter().enumerate() {
            let device = part_device(disk, i + 1);
            devices.push(device.clone());
            st.volumes.push(VolumeInfo {
                device,
                disk: leaf.clone(),
                partition_uuid: format!("uuid-{}", i + 1),
                partition_label: p.label.clone(),
                partition_type_guid: p.part_type.type_guid().to_string(),
                fs_type: None,
                fs_label: None,
                fs_uuid: None,
                size_bytes: p.size_bytes,
            });
        }
        Ok(devices)
    }

    async fn format(&self, device: &str, fs: FsType, label: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.formats.push((device.to_string(), fs, label.to_string()));
        if let Some(v) = st.volumes.iter_mut().find(|v| v.device == device) {
            v.fs_type = Some(fs);
            v.fs_label = Some(label.to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PartType;

    fn disk(name: &str) -> DiskInfo {
        DiskInfo {
            name: name.into(),
            path: format!("/dev/{name}"),
            size_bytes: 8 << 30,
            model: "FAKE".into(),
            serial: "S1".into(),
            rotational: false,
            read_only: false,
        }
    }

    #[tokio::test]
    async fn returns_seeded_disks_and_volumes() {
        let be = FakeBlockBackend::new().with_disk(disk("sda")).with_volume(VolumeInfo {
            device: "/dev/sda1".into(),
            disk: "sda".into(),
            partition_uuid: "u".into(),
            partition_label: "EFI".into(),
            partition_type_guid: "g".into(),
            fs_type: Some(FsType::Vfat),
            fs_label: None,
            fs_uuid: None,
            size_bytes: 1 << 20,
        });
        assert_eq!(be.list_disks().await.unwrap().len(), 1);
        assert_eq!(be.list_volumes().await.unwrap()[0].disk, "sda");
    }

    #[tokio::test]
    async fn simulates_provisioning() {
        let be = FakeBlockBackend::new();
        let plan = vec![
            PartitionPlan {
                label: "EFI".into(),
                part_type: PartType::EfiSystem,
                fs: FsType::Vfat,
                size_bytes: 512 << 20,
            },
            PartitionPlan {
                label: "STATE".into(),
                part_type: PartType::LinuxFilesystem,
                fs: FsType::Ext4,
                size_bytes: 1 << 30,
            },
        ];
        let devs = be.create_partitions("/dev/sda", &plan).await.unwrap();
        assert_eq!(devs, vec!["/dev/sda1".to_string(), "/dev/sda2".to_string()]);
        be.format("/dev/sda2", FsType::Ext4, "STATE").await.unwrap();

        let vols = be.list_volumes().await.unwrap();
        assert_eq!(vols.len(), 2);
        let state = vols.iter().find(|v| v.partition_label == "STATE").unwrap();
        assert_eq!(state.fs_type, Some(FsType::Ext4));
        assert_eq!(be.creates(), vec!["/dev/sda".to_string()]);
        assert_eq!(be.formats().len(), 1);

        be.wipe("/dev/sda").await.unwrap();
        assert!(be.list_volumes().await.unwrap().is_empty());
        assert_eq!(be.wipes(), vec!["/dev/sda".to_string()]);
    }
}
```

- [ ] **Step 3: Test + clippy + commit**

Run: `cargo test -p machined-block` → fsprobe + sysfs unit + fake (2) pass.
Run: `cargo clippy -p machined-block --all-targets -- -D warnings` → clean.
Run: `cargo build --workspace` → PASS (SysfsBlock does NOT yet impl BlockProvisioner — that's fine, it's a separate trait; nothing requires it yet).
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/block/src/lib.rs crates/block/src/fake.rs
git commit -m "feat(block): BlockProvisioner trait + fake provisioning simulation"
```

---

## Task 4: `SysfsBlock` provisioning impl + loopback round-trip

> **SPIKE NOTE — read first.** Writing a GPT via the `gpt` crate is proven (M2b-1's tempfile test).
> The NEW spike risk is (a) making the kernel re-read the partition table so `/dev/sdaN` nodes appear,
> and (b) the byte→LBA sizing in `add_partition`. The code below is best-effort. If the kernel
> re-read approach (`BLKRRPART` ioctl) doesn't work, fall back to shelling `partprobe` or
> `blockdev --rereadpt` — inside `SysfsBlock`, without changing the trait. Report what you changed.
> The loopback round-trip test (Step 4) is the acceptance criterion. If it can't run privileged here
> (no root), it must compile + be `#[ignore]`d; note that you couldn't execute it.

**Files:**
- Modify: `crates/block/src/sysfs.rs`
- Modify: `crates/block/Cargo.toml` (nix already a workspace dep? add the `ioctl` feature if needed)
- Modify: `crates/block/tests/loopback.rs`

- [ ] **Step 1: Add the nix dep for the BLKRRPART ioctl**

In `crates/block/Cargo.toml`, under `[target.'cfg(target_os = "linux")'.dependencies]`, add BOTH:

```toml
nix = { workspace = true }
tokio = { workspace = true }
```

`tokio` is needed by `format`'s `tokio::process` (it was only a dev-dep before). And the
`nix::ioctl_none!` macro requires the `ioctl` feature: add `"ioctl"` to the workspace `nix` features
in the root `Cargo.toml` (CONFIRMED needed in nix 0.29 — the `sys::ioctl` module is gated behind it).

- [ ] **Step 2: Implement BlockProvisioner for SysfsBlock**

Append to `crates/block/src/sysfs.rs`:

```rust
use crate::{BlockProvisioner, FsType, PartType, PartitionPlan};

impl SysfsBlock {
    fn disk_path(&self, disk: &str) -> std::path::PathBuf {
        // Accept either a bare name ("sda") or a full path ("/dev/sda").
        if disk.starts_with('/') {
            std::path::PathBuf::from(disk)
        } else {
            self.dev_root.join(disk)
        }
    }
}

/// Trigger a kernel partition-table re-read so partition device nodes appear.
fn reread_partition_table(path: &Path) -> Result<()> {
    // BLKRRPART ioctl.
    nix::ioctl_none!(blkrrpart, 0x12, 95);
    let f = fs::File::open(path).map_err(|source| BlockError::Io {
        path: path.to_string_lossy().to_string(),
        source,
    })?;
    use std::os::fd::AsRawFd;
    // SAFETY: BLKRRPART takes no argument and only asks the kernel to re-read.
    let res = unsafe { blkrrpart(f.as_raw_fd()) };
    res.map(|_| ()).map_err(|e| BlockError::Wipe {
        disk: path.to_string_lossy().to_string(),
        message: format!("BLKRRPART: {e}"),
    })
}

#[async_trait]
impl BlockProvisioner for SysfsBlock {
    async fn wipe(&self, disk: &str) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let path = self.disk_path(disk);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|source| BlockError::Io {
                path: path.to_string_lossy().to_string(),
                source,
            })?;
        // Zero the first and last 1 MiB (primary + backup GPT live there).
        let zeros = vec![0u8; 1024 * 1024];
        f.write_all(&zeros).map_err(|e| BlockError::Wipe {
            disk: path.to_string_lossy().to_string(),
            message: e.to_string(),
        })?;
        if let Ok(len) = f.seek(SeekFrom::End(0)) {
            if len > zeros.len() as u64 {
                let _ = f.seek(SeekFrom::End(-(zeros.len() as i64)));
                let _ = f.write_all(&zeros);
            }
        }
        f.flush().ok();
        let _ = reread_partition_table(&path);
        Ok(())
    }

    async fn create_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>> {
        let path = self.disk_path(disk);
        let device = path.to_string_lossy().to_string();
        let mut gdisk = gpt::GptConfig::new()
            .writable(true)
            .initialized(false)
            .open(&path)
            .map_err(|e| BlockError::Gpt {
                device: device.clone(),
                message: e.to_string(),
            })?;
        gdisk
            .update_partitions(std::collections::BTreeMap::new())
            .map_err(|e| BlockError::Gpt {
                device: device.clone(),
                message: e.to_string(),
            })?;
        for p in plan {
            let ptype = match p.part_type {
                PartType::EfiSystem => gpt::partition_types::EFI,
                PartType::LinuxFilesystem => gpt::partition_types::LINUX_FS,
            };
            // size 0 → use the rest: the largest free run, in bytes. INVARIANT:
            // a size-0 entry must be the LAST partition in the plan (it claims all
            // remaining free space). fixed_layout() upholds this (EPHEMERAL only).
            let lb = u64::from(*gdisk.logical_block_size());
            let size = if p.size_bytes == 0 {
                gdisk
                    .find_free_sectors()
                    .into_iter()
                    .map(|(_, len)| len)
                    .max()
                    .unwrap_or(0)
                    .saturating_mul(lb)
            } else {
                p.size_bytes
            };
            gdisk
                .add_partition(&p.label, size, ptype, 0, None)
                .map_err(|e| BlockError::Gpt {
                    device: device.clone(),
                    message: e.to_string(),
                })?;
        }
        gdisk.write().map_err(|e| BlockError::Gpt {
            device: device.clone(),
            message: e.to_string(),
        })?;

        // Re-read so partition nodes appear, then derive their paths.
        let _ = reread_partition_table(&path);
        let disk_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| disk.to_string());
        Ok((1..=plan.len())
            .map(|n| self.dev_root.join(part_device(&disk_name, n as u32)).to_string_lossy().to_string())
            .collect())
    }

    async fn format(&self, device: &str, fs: FsType, label: &str) -> Result<()> {
        let (prog, args): (&str, Vec<String>) = match fs {
            FsType::Ext4 => (
                "mkfs.ext4",
                vec!["-F".into(), "-L".into(), label.into(), device.into()],
            ),
            FsType::Vfat => (
                "mkfs.vfat",
                vec!["-n".into(), label.into(), device.into()],
            ),
            FsType::Xfs => (
                "mkfs.xfs",
                vec!["-f".into(), "-L".into(), label.into(), device.into()],
            ),
            FsType::Swap => ("mkswap", vec!["-L".into(), label.into(), device.into()]),
        };
        let status = tokio::process::Command::new(prog)
            .args(&args)
            .status()
            .await
            .map_err(|e| BlockError::Mkfs {
                device: device.to_string(),
                message: format!("{prog}: {e}"),
            })?;
        if !status.success() {
            return Err(BlockError::Mkfs {
                device: device.to_string(),
                message: format!("{prog} exited {status}"),
            });
        }
        Ok(())
    }
}
```

> SPIKE adaptation points: `find_free_sectors`/`logical_block_size`/`add_partition` sizing (the
> "size 0 = rest" computation is best-effort — if `find_free_sectors` differs, compute the rest from
> total sectors minus used), and `reread_partition_table` (BLKRRPART). Adapt inside `SysfsBlock`,
> keep the trait stable, report changes.

- [ ] **Step 3: Build + clippy (spike gate)**

Run: `cargo build -p machined-block`
Expected: PASS. Adapt the gpt/ioctl calls per the SPIKE NOTE until it builds; record changes.

Run: `cargo clippy -p machined-block --all-targets -- -D warnings`
Expected: clean.

Run: `cargo test -p machined-block`
Expected: existing unit tests still pass (no new unit test here — provisioning on a real device is the loopback test).

- [ ] **Step 4: Extend the loopback test with a provisioning round-trip**

Append a second `#[ignore]`d test to `crates/block/tests/loopback.rs`:

```rust
#[tokio::test]
#[ignore = "requires root + losetup + mkfs"]
async fn provisions_loopback_disk() {
    use machined_block::{BlockProvisioner, FsType, PartType, PartitionPlan, SysfsBlock};

    let img = std::env::temp_dir().join("mnd-prov.img");
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(256 * 1024 * 1024).unwrap();
    }
    let out = std::process::Command::new("losetup")
        .args(["-fP", "--show", img.to_str().unwrap()])
        .output()
        .expect("losetup");
    assert!(out.status.success(), "losetup failed");
    let loopdev = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let be = SysfsBlock::new();
    let plan = vec![
        PartitionPlan {
            label: "EFI".into(),
            part_type: PartType::EfiSystem,
            fs: FsType::Vfat,
            size_bytes: 64 * 1024 * 1024,
        },
        PartitionPlan {
            label: "STATE".into(),
            part_type: PartType::LinuxFilesystem,
            fs: FsType::Ext4,
            size_bytes: 0,
        },
    ];
    be.wipe(&loopdev).await.unwrap();
    let devs = be.create_partitions(&loopdev, &plan).await.unwrap();
    be.format(&devs[0], FsType::Vfat, "EFI").await.unwrap();
    be.format(&devs[1], FsType::Ext4, "STATE").await.unwrap();

    let vols = be.list_volumes().await.unwrap();
    let loopname = loopdev.trim_start_matches("/dev/").to_string();
    let found_state = vols
        .iter()
        .any(|v| v.disk == loopname && v.partition_label == "STATE" && v.fs_type == Some(FsType::Ext4));

    let _ = std::process::Command::new("losetup").args(["-d", &loopdev]).status();
    std::fs::remove_file(&img).ok();

    assert!(found_state, "STATE ext4 partition discovered after provisioning");
}
```

- [ ] **Step 5: Default suite (loopback ignored) + best-effort privileged run + commit**

Run: `cargo test -p machined-block` → unit tests pass; both loopback tests ignored.
Best-effort: `sudo -E cargo test -p machined-block --test loopback -- --ignored --nocapture`. If no root, note it (do not claim a pass).
Run: `cargo build --workspace` / `cargo clippy --all-targets --all-features -- -D warnings` / `cargo fmt --all -- --check` → all clean.

```bash
git add crates/block Cargo.toml Cargo.lock
git commit -m "feat(block): SysfsBlock provisioning (wipe/partition/format) + loopback round-trip"
```

---

## Task 5: `plan_provisioning` guard + `VolumeProvisionerController`

**Files:**
- Create: `crates/controllers/src/block/provision.rs`
- Modify: `crates/controllers/src/block/mod.rs`

- [ ] **Step 1: Write the pure guard + its exhaustive tests**

Create `crates/controllers/src/block/provision.rs`:

```rust
//! The block provisioning controller and its pure safety guard.

use std::sync::Arc;

use async_trait::async_trait;
use machined_block::{BlockProvisioner, FsType, PartType, PartitionPlan};
use machined_config::Provider;
use machined_resources::{
    DiscoveredVolume, Resource, ResourceObject, ResourceType, VolumePhase, VolumeStatus,
};
use machined_runtime_core::{
    reconcile_owned, Controller, Input, InputKind, Output, OutputKind, ReconcileCtx,
};
use tracing::{error, info};

use super::{ctl, NS};

const OWNER: &str = "volume-provisioner";

/// The fixed labels this OS lays out.
const LABELS: [&str; 3] = ["EFI", "STATE", "EPHEMERAL"];

/// The decision the safety guard reaches about an install disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvisionDecision {
    /// The disk already carries our exact layout — nothing to do.
    Skip,
    /// The disk is blank, or wipe was requested — lay out fresh.
    Provision,
    /// The disk has foreign data and wipe was not requested — refuse.
    RefuseForeign,
}

/// Decide what to do with `install_disk`, given the discovered volumes and the
/// wipe flag. PURE — no I/O. This is the single source of the destructive
/// decision.
pub fn plan_provisioning(
    install_disk: &str,
    wipe: bool,
    discovered: &[DiscoveredVolume],
) -> ProvisionDecision {
    // Match a discovered volume to this disk by parent-disk name or device path.
    let leaf = install_disk.rsplit('/').next().unwrap_or(install_disk);
    let on_disk: Vec<&DiscoveredVolume> = discovered
        .iter()
        .filter(|v| v.disk == leaf || v.device == install_disk)
        .collect();

    if on_disk.is_empty() {
        return ProvisionDecision::Provision; // blank disk
    }

    let labels: Vec<&str> = on_disk.iter().map(|v| v.partition_label.as_str()).collect();
    let is_ours = LABELS.iter().all(|l| labels.contains(l))
        && labels.iter().all(|l| LABELS.contains(l));
    if is_ours {
        return ProvisionDecision::Skip;
    }

    if wipe {
        ProvisionDecision::Provision
    } else {
        ProvisionDecision::RefuseForeign
    }
}

/// The fixed GPT layout this OS provisions.
pub fn fixed_layout() -> Vec<PartitionPlan> {
    vec![
        PartitionPlan {
            label: "EFI".into(),
            part_type: PartType::EfiSystem,
            fs: FsType::Vfat,
            size_bytes: 512 * 1024 * 1024,
        },
        PartitionPlan {
            label: "STATE".into(),
            part_type: PartType::LinuxFilesystem,
            fs: FsType::Ext4,
            size_bytes: 1024 * 1024 * 1024,
        },
        PartitionPlan {
            label: "EPHEMERAL".into(),
            part_type: PartType::LinuxFilesystem,
            fs: FsType::Ext4,
            size_bytes: 0, // rest
        },
    ]
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    fn vol(disk: &str, label: &str) -> DiscoveredVolume {
        DiscoveredVolume {
            device: format!("/dev/{disk}1"),
            disk: disk.into(),
            partition_uuid: "u".into(),
            partition_label: label.into(),
            partition_type_guid: "g".into(),
            fs_type: None,
            fs_label: None,
            fs_uuid: None,
            size_bytes: 1 << 20,
        }
    }

    #[test]
    fn blank_disk_provisions() {
        assert_eq!(
            plan_provisioning("/dev/sda", false, &[]),
            ProvisionDecision::Provision
        );
    }

    #[test]
    fn our_exact_layout_skips() {
        let d = vec![vol("sda", "EFI"), vol("sda", "STATE"), vol("sda", "EPHEMERAL")];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::Skip
        );
    }

    #[test]
    fn foreign_no_wipe_refuses() {
        let d = vec![vol("sda", "WINDOWS")];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::RefuseForeign
        );
    }

    #[test]
    fn foreign_with_wipe_provisions() {
        let d = vec![vol("sda", "WINDOWS")];
        assert_eq!(
            plan_provisioning("/dev/sda", true, &d),
            ProvisionDecision::Provision
        );
    }

    #[test]
    fn partial_our_layout_is_foreign() {
        // Only STATE present (missing EFI/EPHEMERAL) → not our exact layout.
        let d = vec![vol("sda", "STATE")];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::RefuseForeign
        );
    }

    #[test]
    fn volumes_on_other_disk_ignored() {
        let d = vec![vol("sdb", "WINDOWS")];
        assert_eq!(
            plan_provisioning("/dev/sda", false, &d),
            ProvisionDecision::Provision
        );
    }
}
```

- [ ] **Step 2: Run the guard tests**

Run: `cargo test -p machined-controllers guard_tests`
Expected: FAIL to compile first (module not declared) — add `pub mod provision;` per Step 4, then all six pass. (You may add the module declaration now to compile.)

- [ ] **Step 3: Add the controller**

Append to `crates/controllers/src/block/provision.rs`:

```rust
pub struct VolumeProvisionerController {
    backend: Arc<dyn BlockProvisioner>,
    provider: Provider,
}

impl VolumeProvisionerController {
    pub fn new(backend: Arc<dyn BlockProvisioner>, provider: Provider) -> Self {
        Self { backend, provider }
    }
}

#[async_trait]
impl Controller for VolumeProvisionerController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        // Re-evaluate when discovery changes.
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::DiscoveredVolume,
            kind: InputKind::Weak,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::VolumeStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let Some(install) = self.provider.install() else {
            return Ok(()); // no install target configured
        };
        let disk = install.disk.clone();

        let discovered: Vec<DiscoveredVolume> = ctx
            .state
            .list(NS, ResourceType::DiscoveredVolume)
            .into_iter()
            .filter_map(|o| match o.spec {
                Resource::DiscoveredVolume(v) => Some(v),
                _ => None,
            })
            .collect();

        match plan_provisioning(&disk, install.wipe, &discovered) {
            ProvisionDecision::RefuseForeign => {
                error!(disk = %disk, "refusing to provision: disk has foreign data and wipe is false");
                return Err(ctl(format!(
                    "install disk {disk} has foreign data; set install.wipe to overwrite"
                )));
            }
            ProvisionDecision::Skip => {
                info!(disk = %disk, "install disk already provisioned");
                let vols = provisioned_status_from_discovered(&disk, &discovered);
                reconcile_owned(&ctx.state, OWNER, NS, ResourceType::VolumeStatus, vols)?;
            }
            ProvisionDecision::Provision => {
                info!(disk = %disk, wipe = install.wipe, "provisioning install disk");
                if !discovered.is_empty() && install.wipe {
                    self.backend.wipe(&disk).await.map_err(ctl)?;
                }
                let layout = fixed_layout();
                let devices = self.backend.create_partitions(&disk, &layout).await.map_err(ctl)?;
                let mut statuses = Vec::new();
                for (plan, device) in layout.iter().zip(devices.iter()) {
                    self.backend
                        .format(device, plan.fs, &plan.label)
                        .await
                        .map_err(ctl)?;
                    statuses.push(volume_status_obj(
                        &plan.label,
                        device,
                        plan.fs.as_str(),
                        &plan.label,
                        VolumePhase::Provisioned,
                    ));
                }
                reconcile_owned(&ctx.state, OWNER, NS, ResourceType::VolumeStatus, statuses)?;
            }
        }
        Ok(())
    }
}

fn volume_status_obj(
    name: &str,
    device: &str,
    fs: &str,
    label: &str,
    phase: VolumePhase,
) -> ResourceObject {
    ResourceObject::new(
        NS,
        name,
        Resource::VolumeStatus(VolumeStatus {
            name: name.to_string(),
            device: device.to_string(),
            fs: fs.to_string(),
            label: label.to_string(),
            phase,
        }),
    )
}

/// Build VolumeStatus for an already-provisioned disk from discovery.
fn provisioned_status_from_discovered(disk: &str, discovered: &[DiscoveredVolume]) -> Vec<ResourceObject> {
    let leaf = disk.rsplit('/').next().unwrap_or(disk);
    discovered
        .iter()
        .filter(|v| v.disk == leaf)
        .filter(|v| LABELS.contains(&v.partition_label.as_str()))
        .map(|v| {
            volume_status_obj(
                &v.partition_label,
                &v.device,
                v.fs_type.as_deref().unwrap_or(""),
                &v.partition_label,
                VolumePhase::Provisioned,
            )
        })
        .collect()
}

#[cfg(test)]
mod controller_tests {
    use super::*;
    use machined_block::{DiskInfo, FakeBlockBackend, FsType, VolumeInfo};
    use machined_config::{InstallSection, MachineConfig, MachineSection};
    use machined_resources::Resource as Res;
    use machined_runtime_core::{ReconcileCtx, State};

    fn provider(disk: &str, wipe: bool) -> Provider {
        Provider::new(MachineConfig {
            machine: MachineSection {
                hostname: None,
                sysctls: vec![],
                services: vec![],
                network: Default::default(),
                install: Some(InstallSection {
                    disk: disk.into(),
                    wipe,
                }),
            },
        })
    }

    fn seed_discovered(state: &State, disk: &str, label: &str) {
        state
            .create(ResourceObject::new(
                NS,
                format!("{disk}-{label}"),
                Res::DiscoveredVolume(DiscoveredVolume {
                    device: format!("/dev/{disk}1"),
                    disk: disk.into(),
                    partition_uuid: "u".into(),
                    partition_label: label.into(),
                    partition_type_guid: "g".into(),
                    fs_type: None,
                    fs_label: None,
                    fs_uuid: None,
                    size_bytes: 1 << 20,
                }),
            ))
            .unwrap();
    }

    #[tokio::test]
    async fn blank_disk_gets_provisioned() {
        let backend = Arc::new(FakeBlockBackend::new().with_disk(DiskInfo {
            name: "sda".into(),
            path: "/dev/sda".into(),
            size_bytes: 8 << 30,
            model: "M".into(),
            serial: "S".into(),
            rotational: false,
            read_only: false,
        }));
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeProvisionerController::new(backend.clone(), provider("/dev/sda", false));
        c.reconcile(&ctx).await.unwrap();

        // Three partitions created + formatted; three VolumeStatus published.
        assert_eq!(backend.creates(), vec!["/dev/sda".to_string()]);
        assert_eq!(backend.formats().len(), 3);
        assert_eq!(state.list(NS, ResourceType::VolumeStatus).len(), 3);
    }

    #[tokio::test]
    async fn foreign_disk_without_wipe_makes_no_destructive_call() {
        let backend = Arc::new(FakeBlockBackend::new());
        let state = State::new();
        seed_discovered(&state, "sda", "WINDOWS");
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeProvisionerController::new(backend.clone(), provider("/dev/sda", false));
        let res = c.reconcile(&ctx).await;

        assert!(res.is_err(), "refuses foreign disk");
        // CRITICAL: no destructive operation was performed.
        assert!(backend.wipes().is_empty());
        assert!(backend.creates().is_empty());
        assert!(backend.formats().is_empty());
        assert_eq!(state.list(NS, ResourceType::VolumeStatus).len(), 0);
    }

    #[tokio::test]
    async fn idempotent_second_reconcile_skips() {
        // First provision the (fake) disk via the controller, then re-run with
        // discovery reflecting our layout → Skip (no second create).
        let backend = Arc::new(FakeBlockBackend::new());
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeProvisionerController::new(backend.clone(), provider("/dev/sda", false));
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(backend.creates().len(), 1);

        // Simulate discovery seeing our layout now.
        for label in ["EFI", "STATE", "EPHEMERAL"] {
            seed_discovered(&state, "sda", label);
        }
        c.reconcile(&ctx).await.unwrap();
        // Still only one create — the second pass Skipped.
        assert_eq!(backend.creates().len(), 1);
    }

    #[tokio::test]
    async fn foreign_with_wipe_wipes_then_provisions() {
        let backend = Arc::new(FakeBlockBackend::new());
        let state = State::new();
        seed_discovered(&state, "sda", "WINDOWS");
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = VolumeProvisionerController::new(backend.clone(), provider("/dev/sda", true));
        c.reconcile(&ctx).await.unwrap();
        assert_eq!(backend.wipes(), vec!["/dev/sda".to_string()]);
        assert_eq!(backend.creates().len(), 1);
        assert_eq!(backend.formats().len(), 3);
    }

    #[allow(unused_imports)]
    use FsType as _; // keep FsType import used if the compiler prunes it
}
```

> Note: remove the `use FsType as _;` line if it triggers a clippy warning; it's only there to avoid
> an unused-import error if `FsType` ends up unreferenced in the test module. If `FsType` is used,
> delete that line.

- [ ] **Step 4: Wire the module + run tests**

In `crates/controllers/src/block/mod.rs`, add:

```rust
pub mod discovery;
pub mod provision;

pub use discovery::DiskDiscoveryController;
pub use provision::{plan_provisioning, ProvisionDecision, VolumeProvisionerController};
```

Run: `cargo test -p machined-controllers` → guard_tests (6) + controller_tests (4) + existing pass.
Run: `cargo clippy -p machined-controllers --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/controllers
git commit -m "feat(controllers): plan_provisioning guard + VolumeProvisionerController"
```

---

## Task 6: machined wiring + e2e

**Files:**
- Modify: `crates/machined/src/main.rs`
- Create: `crates/machined/tests/provision.rs`

- [ ] **Step 1: Build one block backend and coerce to both trait objects**

In `crates/machined/src/main.rs`, replace the `build_block_backend()` helper and its call site so a single concrete backend is shared by both the discovery and provisioner controllers.

Replace the existing helper:

```rust
fn build_block_backend() -> Arc<dyn machined_block::BlockBackend> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_block::SysfsBlock::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_block::FakeBlockBackend::new())
    }
}
```

with a builder of the concrete provisioner that upcasts cleanly:

```rust
fn build_block_provisioner() -> Arc<dyn machined_block::BlockProvisioner> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_block::SysfsBlock::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_block::FakeBlockBackend::new())
    }
}
```

Add the import:

```rust
use machined_controllers::block::{DiskDiscoveryController, VolumeProvisionerController};
```

In `run_daemon`, replace the discovery registration block. Where it currently does:

```rust
    runtime.register(Box::new(DiskDiscoveryController::new(build_block_backend())));
```

use one shared backend for both controllers (and register the provisioner after discovery):

```rust
    let block = build_block_provisioner();
    // BlockProvisioner is a supertrait of BlockBackend; a fresh trait object for
    // discovery is built from the same concrete type.
    runtime.register(Box::new(DiskDiscoveryController::new(build_block_backend_for_discovery())));
    runtime.register(Box::new(VolumeProvisionerController::new(
        block,
        provider.clone(),
    )));
```

Add the discovery-backend helper (kept separate so its return type is `Arc<dyn BlockBackend>`):

```rust
fn build_block_backend_for_discovery() -> Arc<dyn machined_block::BlockBackend> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_block::SysfsBlock::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_block::FakeBlockBackend::new())
    }
}
```

> Two backend instances is fine: on Linux both read/write the same kernel; on non-Linux the fakes are
> independent but the daemon is non-functional there anyway. (A later refactor can share one concrete
> `Arc` and upcast; that requires trait-upcasting coercion and is out of scope here.)

- [ ] **Step 2: Build + smoke test**

Run: `cargo build --workspace` → PASS.
Run: `cargo run -p machined -- version` → `machined 0.1.0`.

- [ ] **Step 3: Write the e2e provisioning test**

Create `crates/machined/tests/provision.rs`:

```rust
//! End-to-end: the provisioner controller on the real Runtime against a fake
//! backend provisions a blank install disk and publishes VolumeStatus. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_block::{DiskInfo, FakeBlockBackend};
use machined_config::{InstallSection, MachineConfig, MachineSection, Provider};
use machined_controllers::block::{VolumeProvisionerController, NS};
use machined_resources::ResourceType;
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn provisions_blank_install_disk() {
    let backend = Arc::new(FakeBlockBackend::new().with_disk(DiskInfo {
        name: "vda".into(),
        path: "/dev/vda".into(),
        size_bytes: 16 << 30,
        model: "VIRT".into(),
        serial: "V1".into(),
        rotational: false,
        read_only: false,
    }));

    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: Default::default(),
            install: Some(InstallSection {
                disk: "/dev/vda".into(),
                wipe: false,
            }),
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(VolumeProvisionerController::new(
        backend.clone(),
        Provider::new(config),
    )));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut ok = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if state.list(NS, ResourceType::VolumeStatus).len() == 3 {
            ok = true;
            break;
        }
    }
    assert!(ok, "provisioner did not publish 3 VolumeStatus");
    assert_eq!(backend.creates().len(), 1);
    assert_eq!(backend.formats().len(), 3);

    shutdown.cancel();
    let _ = handle.await;
}
```

- [ ] **Step 4: Full gate + commit**

Run: `cargo test -p machined --test provision` → PASS.
Run: `make pre-commit` → fmt + clippy -D warnings + full workspace test green (loopback tests ignored).

```bash
git add crates/machined
git commit -m "feat(machined): register VolumeProvisionerController + e2e provisioning test"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** `BlockProvisioner` wipe/create_partitions/format on a supertrait (Task 3) ✓; `SysfsBlock` real impl + gated loopback round-trip (Task 4) ✓; `install` config (Task 2) ✓; `VolumeStatus` (Task 1) ✓; pure `plan_provisioning` guard with exhaustive tests + `VolumeProvisionerController` that runs destructive ops ONLY on `Provision` (Task 5) ✓; controller tests assert **zero destructive calls** on RefuseForeign + idempotent Skip (Task 5) ✓; machined wiring + root-free e2e (Task 6) ✓.
- **Design refinement vs spec:** the spec said "extend `BlockBackend`"; this uses a `BlockProvisioner: BlockBackend` **supertrait** instead — cleaner (read-only backends don't carry destructive methods) and keeps the build green between the fake-impl task (T3) and the SysfsBlock spike (T4). The fixed layout, guard semantics, and VolumeStatus are exactly as specified.
- **Safety:** the destructive `wipe`/`create_partitions`/`format` are reachable ONLY inside the `ProvisionDecision::Provision` match arm; `RefuseForeign` returns an error before any backend call (asserted by `foreign_disk_without_wipe_makes_no_destructive_call`). The guard is pure and exhaustively tested.
- **Deliberate M2b-2a limits (per spec):** no mount (M2b-2b), no encryption/LVM/resize, fixed layout only, bootloader/EFI-content not written. The `gpt`-write + kernel-re-read spike is isolated to `SysfsBlock`; the trait/guard/controller are deterministic and fully tested without a real device.
- **Type consistency:** `BlockProvisioner`/`PartitionPlan`/`PartType`/`FsType` (block) ↔ `VolumeStatus`/`VolumePhase`/`DiscoveredVolume` (resources) ↔ `InstallSection` (config); `reconcile_owned`/`Controller`/`Input(Weak)`/`Output(Exclusive)` (runtime-core). The controller reads `DiscoveredVolume` from the store (populated by M2b-1's discovery controller) — in machined both are registered, discovery first.
- **Placeholder scan:** none; Task 4's `gpt`/ioctl code is real best-effort with an explicit spike protocol.
