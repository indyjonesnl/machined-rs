# machined-rs M2b-1 — Block Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M0/M1/M2a merged to `main`. Work on branch `spec/machined-rs-m2b1-block-discovery`.

**Goal:** Read-only block discovery — a `block` crate (`BlockBackend` trait + pure-Rust `SysfsBlock` + fake), `DiskStatus`/`DiscoveredVolume` resources, and a `DiskDiscoveryController` that populates the store with the node's disks/partitions/filesystems via `reconcile_owned`. No destructive operations.

**Architecture:** Same trait/real/fake pattern as `netlink`. `SysfsBlock` enumerates `/sys/block`, reads GPT partition tables with the `gpt` crate, and probes partition filesystem type by magic bytes (ext4/vfat/xfs/swap) — all read-only. The discovery controller publishes observed state via `reconcile_owned` so vanished devices are GC'd. Unit tests are root-free (injectable sysfs root, in-test GPT tempfile, magic-byte fixtures, fake backend); one loopback integration test (gated, root) covers the real device path.

**Tech Stack:** `gpt` crate (GPT reading), `async-trait`, `tokio`, `thiserror`, `tracing`. Pure `std` for sysfs + magic probing.

---

## File Structure

```
crates/resources/src/block.rs       # NEW: DiskStatus + DiscoveredVolume
crates/resources/src/{metadata,resource,lib}.rs   # MODIFY: 2 new variants + re-exports
crates/block/
├── Cargo.toml                       # NEW
└── src/
    ├── lib.rs                       # NEW: FsType, DiskInfo, VolumeInfo, BlockError, BlockBackend trait
    ├── fake.rs                      # NEW: FakeBlockBackend
    ├── fsprobe.rs                   # NEW: probe_fs (pure magic detection)
    └── sysfs.rs                     # NEW (Linux): SysfsBlock real backend
crates/block/tests/loopback.rs       # NEW: gated loopback integration test
crates/controllers/src/block/
├── mod.rs                           # NEW
└── discovery.rs                     # NEW: DiskDiscoveryController
crates/controllers/src/lib.rs        # MODIFY: pub mod block
crates/machined/src/main.rs          # MODIFY: build block backend + register controller
crates/machined/tests/block.rs       # NEW: e2e discovery against fake backend
```

---

## Task 1: `DiskStatus` + `DiscoveredVolume` resources

**Files:**
- Create: `crates/resources/src/block.rs`
- Modify: `crates/resources/src/metadata.rs`
- Modify: `crates/resources/src/resource.rs`
- Modify: `crates/resources/src/lib.rs`

- [ ] **Step 1: Add the ResourceType variants**

In `crates/resources/src/metadata.rs`, add `DiskStatus` and `DiscoveredVolume` to the `ResourceType` enum (after `RouteStatus`) and to its `Display` match:

```rust
    RouteStatus,
    DiskStatus,
    DiscoveredVolume,
}
```

```rust
            ResourceType::RouteStatus => "RouteStatus",
            ResourceType::DiskStatus => "DiskStatus",
            ResourceType::DiscoveredVolume => "DiscoveredVolume",
        };
```

- [ ] **Step 2: Create the block resource specs with a test**

Create `crates/resources/src/block.rs`:

```rust
//! Block-storage resources (observed state from discovery). Pure data.

/// An enumerated block device (disk).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskStatus {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub model: String,
    pub serial: String,
    pub rotational: bool,
    pub read_only: bool,
}

/// A discovered partition + its probed filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredVolume {
    pub device: String,
    pub disk: String,
    pub partition_uuid: String,
    pub partition_label: String,
    pub partition_type_guid: String,
    pub fs_type: Option<String>,
    pub fs_label: Option<String>,
    pub fs_uuid: Option<String>,
    pub size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs() {
        let v = DiscoveredVolume {
            device: "/dev/sda1".into(),
            disk: "sda".into(),
            partition_uuid: "uuid".into(),
            partition_label: "EFI".into(),
            partition_type_guid: "guid".into(),
            fs_type: Some("vfat".into()),
            fs_label: None,
            fs_uuid: None,
            size_bytes: 512 * 2048,
        };
        assert_eq!(v.fs_type.as_deref(), Some("vfat"));
    }
}
```

- [ ] **Step 3: Add the Resource enum variants**

In `crates/resources/src/resource.rs`, extend the import to include the block types:

```rust
use crate::block::{DiscoveredVolume, DiskStatus};
```

Add the two variants to the `Resource` enum (after `RouteStatus`) and to `resource_type`:

```rust
    RouteStatus(RouteStatus),
    DiskStatus(DiskStatus),
    DiscoveredVolume(DiscoveredVolume),
}
```

```rust
            Resource::RouteStatus(_) => ResourceType::RouteStatus,
            Resource::DiskStatus(_) => ResourceType::DiskStatus,
            Resource::DiscoveredVolume(_) => ResourceType::DiscoveredVolume,
        }
```

- [ ] **Step 4: Wire module + re-exports**

In `crates/resources/src/lib.rs`, add `pub mod block;` (after `pub mod metadata;`... keep alphabetical-ish: put `pub mod block;` first) and re-export:

```rust
pub mod block;
pub mod metadata;
pub mod network;
pub mod resource;

pub use block::{DiscoveredVolume, DiskStatus};
```

(Keep the existing `metadata`/`network`/`resource` re-export lines unchanged.)

- [ ] **Step 5: Test + clippy + commit**

Run: `cargo test -p machined-resources` → existing + `block::tests::constructs` pass.
Run: `cargo clippy -p machined-resources --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add crates/resources
git commit -m "feat(resources): DiskStatus + DiscoveredVolume block resources"
```

---

## Task 2: `block` crate — trait + types + fake

**Files:**
- Modify: `Cargo.toml` (members + deps)
- Create: `crates/block/Cargo.toml`
- Create: `crates/block/src/lib.rs`
- Create: `crates/block/src/fake.rs`

- [ ] **Step 1: Add workspace member + deps**

In root `Cargo.toml`, add `"crates/block"` to `members`. Add to `[workspace.dependencies]`:

```toml
gpt = "3.1"

machined-block = { path = "crates/block" }
```

> The exact `gpt` version is confirmed during Task 4's spike; `3.1` is the starting point.

- [ ] **Step 2: Create the block crate manifest**

Create `crates/block/Cargo.toml`:

```toml
[package]
name = "machined-block"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
machined-resources.workspace = true
async-trait.workspace = true
thiserror.workspace = true
tracing.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
gpt.workspace = true

[dev-dependencies]
tokio = { workspace = true }
```

- [ ] **Step 3: Write the crate root (types + trait)**

Create `crates/block/src/lib.rs`:

```rust
//! Block-device discovery backend: a `BlockBackend` trait abstracting disk and
//! partition enumeration, with a pure-Rust `SysfsBlock` implementation (Linux)
//! and an in-memory fake. Read-only in M2b-1.

pub mod fake;
pub mod fsprobe;
#[cfg(target_os = "linux")]
pub mod sysfs;

use async_trait::async_trait;

pub use fake::FakeBlockBackend;
pub use fsprobe::{probe_fs, FsProbe};
#[cfg(target_os = "linux")]
pub use sysfs::SysfsBlock;

#[derive(thiserror::Error, Debug)]
pub enum BlockError {
    #[error("io {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("gpt {device}: {message}")]
    Gpt { device: String, message: String },
}

pub type Result<T> = std::result::Result<T, BlockError>;

/// Probed filesystem type (the set M2b-1 recognises).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsType {
    Ext4,
    Vfat,
    Xfs,
    Swap,
}

impl FsType {
    pub fn as_str(self) -> &'static str {
        match self {
            FsType::Ext4 => "ext4",
            FsType::Vfat => "vfat",
            FsType::Xfs => "xfs",
            FsType::Swap => "swap",
        }
    }
}

/// An enumerated disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskInfo {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub model: String,
    pub serial: String,
    pub rotational: bool,
    pub read_only: bool,
}

/// A discovered partition + its probed filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    pub device: String,
    pub disk: String,
    pub partition_uuid: String,
    pub partition_label: String,
    pub partition_type_guid: String,
    pub fs_type: Option<FsType>,
    pub fs_label: Option<String>,
    pub fs_uuid: Option<String>,
    pub size_bytes: u64,
}

/// Read-only enumeration of disks and their partitions/filesystems.
#[async_trait]
pub trait BlockBackend: Send + Sync {
    async fn list_disks(&self) -> Result<Vec<DiskInfo>>;
    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>>;
}
```

- [ ] **Step 4: Write the fake with tests**

Create `crates/block/src/fake.rs`:

```rust
//! In-memory `BlockBackend` for root-free tests.

use async_trait::async_trait;

use crate::{BlockBackend, DiskInfo, Result, VolumeInfo};

#[derive(Default)]
pub struct FakeBlockBackend {
    disks: Vec<DiskInfo>,
    volumes: Vec<VolumeInfo>,
}

impl FakeBlockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_disk(mut self, disk: DiskInfo) -> Self {
        self.disks.push(disk);
        self
    }

    pub fn with_volume(mut self, volume: VolumeInfo) -> Self {
        self.volumes.push(volume);
        self
    }
}

#[async_trait]
impl BlockBackend for FakeBlockBackend {
    async fn list_disks(&self) -> Result<Vec<DiskInfo>> {
        Ok(self.disks.clone())
    }
    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>> {
        Ok(self.volumes.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str) -> DiskInfo {
        DiskInfo {
            name: name.into(),
            path: format!("/dev/{name}"),
            size_bytes: 1 << 30,
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
            fs_type: Some(crate::FsType::Vfat),
            fs_label: None,
            fs_uuid: None,
            size_bytes: 1 << 20,
        });
        assert_eq!(be.list_disks().await.unwrap().len(), 1);
        assert_eq!(be.list_volumes().await.unwrap()[0].disk, "sda");
    }
}
```

- [ ] **Step 5: Create a placeholder fsprobe + sysfs so the crate compiles**

Create `crates/block/src/fsprobe.rs` (filled in Task 3):

```rust
//! Filesystem magic-byte probing. Filled in Task 3.

/// Probed filesystem identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsProbe {
    pub fs_type: crate::FsType,
    pub label: Option<String>,
    pub uuid: Option<String>,
}

/// Probe a filesystem from the leading bytes of a device. Filled in Task 3.
pub fn probe_fs(_buf: &[u8]) -> Option<FsProbe> {
    None
}
```

Create `crates/block/src/sysfs.rs` (filled in Task 4):

```rust
// Real SysfsBlock backend lands in Task 4.
```

Temporarily comment the `sysfs` module + `SysfsBlock` re-export in `lib.rs` (Task 4 restores):

```rust
pub mod fake;
pub mod fsprobe;
// #[cfg(target_os = "linux")] pub mod sysfs;  // Task 4
```

```rust
pub use fake::FakeBlockBackend;
pub use fsprobe::{probe_fs, FsProbe};
// #[cfg(target_os = "linux")] pub use sysfs::SysfsBlock;  // Task 4
```

- [ ] **Step 6: Test + clippy + commit**

Run: `cargo test -p machined-block` → fake test passes.
Run: `cargo clippy -p machined-block --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add Cargo.toml Cargo.lock crates/block
git commit -m "feat(block): BlockBackend trait + types + in-memory fake"
```

---

## Task 3: `fsprobe` — filesystem magic detection

**Files:**
- Modify: `crates/block/src/fsprobe.rs`

- [ ] **Step 1: Write the failing tests**

Replace `crates/block/src/fsprobe.rs` with:

```rust
//! Filesystem identification by magic bytes, for ext4, vfat, xfs, and swap.
//! Pure and root-free: callers pass the leading bytes of a partition device.

use crate::FsType;

/// Probed filesystem identity. `label`/`uuid` are populated where trivially
/// available (ext4); other filesystems report only the type in M2b-1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsProbe {
    pub fs_type: FsType,
    pub label: Option<String>,
    pub uuid: Option<String>,
}

/// Identify the filesystem from the leading bytes of a device (needs >= 4096
/// bytes to detect swap; less is fine for the others). Returns `None` for an
/// unrecognised filesystem.
pub fn probe_fs(buf: &[u8]) -> Option<FsProbe> {
    // ext2/3/4: superblock at byte 1024, s_magic 0xEF53 (LE) at offset 0x38.
    if buf.len() >= 1082 && buf[1080] == 0x53 && buf[1081] == 0xEF {
        let uuid = (buf.len() >= 1144).then(|| format_uuid(&buf[1128..1144]));
        let label = (buf.len() >= 1160)
            .then(|| read_cstr(&buf[1144..1160]))
            .flatten();
        return Some(FsProbe {
            fs_type: FsType::Ext4,
            label,
            uuid,
        });
    }
    // xfs: "XFSB" at byte 0.
    if buf.len() >= 4 && &buf[0..4] == b"XFSB" {
        return Some(FsProbe {
            fs_type: FsType::Xfs,
            label: None,
            uuid: None,
        });
    }
    // vfat: boot-sector signature 0x55AA at 510, FAT type string at 54 or 82.
    if buf.len() >= 512 && buf[510] == 0x55 && buf[511] == 0xAA {
        let fat32 = buf.len() >= 85 && &buf[82..85] == b"FAT";
        let fat16 = buf.len() >= 57 && &buf[54..57] == b"FAT";
        if fat32 || fat16 {
            return Some(FsProbe {
                fs_type: FsType::Vfat,
                label: None,
                uuid: None,
            });
        }
    }
    // swap: signature in the last 10 bytes of the first 4096-byte page.
    if buf.len() >= 4096 {
        let sig = &buf[4086..4096];
        if sig == b"SWAPSPACE2" || sig == b"SWAP-SPACE" {
            return Some(FsProbe {
                fs_type: FsType::Swap,
                label: None,
                uuid: None,
            });
        }
    }
    None
}

/// Format 16 raw bytes as a hyphenated UUID string.
fn format_uuid(b: &[u8]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Read a nul-terminated label; `None` if empty.
fn read_cstr(b: &[u8]) -> Option<String> {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let s = String::from_utf8_lossy(&b[..end]).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeroed(n: usize) -> Vec<u8> {
        vec![0u8; n]
    }

    #[test]
    fn detects_ext4_with_label_and_uuid() {
        let mut buf = zeroed(2048);
        buf[1080] = 0x53;
        buf[1081] = 0xEF;
        buf[1128..1144].copy_from_slice(&[
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ]);
        buf[1144..1148].copy_from_slice(b"root");
        let p = probe_fs(&buf).unwrap();
        assert_eq!(p.fs_type, FsType::Ext4);
        assert_eq!(p.label.as_deref(), Some("root"));
        assert_eq!(p.uuid.as_deref(), Some("01234567-89ab-cdef-fedc-ba9876543210"));
    }

    #[test]
    fn detects_xfs() {
        let mut buf = zeroed(512);
        buf[0..4].copy_from_slice(b"XFSB");
        assert_eq!(probe_fs(&buf).unwrap().fs_type, FsType::Xfs);
    }

    #[test]
    fn detects_vfat() {
        let mut buf = zeroed(512);
        buf[510] = 0x55;
        buf[511] = 0xAA;
        buf[82..85].copy_from_slice(b"FAT");
        assert_eq!(probe_fs(&buf).unwrap().fs_type, FsType::Vfat);
    }

    #[test]
    fn detects_swap() {
        let mut buf = zeroed(4096);
        buf[4086..4096].copy_from_slice(b"SWAPSPACE2");
        assert_eq!(probe_fs(&buf).unwrap().fs_type, FsType::Swap);
    }

    #[test]
    fn unknown_is_none() {
        assert!(probe_fs(&zeroed(4096)).is_none());
    }
}
```

- [ ] **Step 2: Test + clippy + commit**

Run: `cargo test -p machined-block fsprobe` → all five tests pass.
Run: `cargo clippy -p machined-block --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add crates/block/src/fsprobe.rs
git commit -m "feat(block): filesystem magic probing (ext4/vfat/xfs/swap)"
```

---

## Task 4: `SysfsBlock` real backend + loopback integration test

> **SPIKE NOTE — read first.** The `gpt` crate API may differ from the code below (versions have
> shifted `GptConfig`/`Partition` field names and `open` signatures). If it does not compile against
> the resolved version, that is expected spike work: adjust the `read_partitions` internals to match
> the actual API — the operation (open a disk read-only, list partitions with name/guid/type-guid/LBA
> range) is stable. **Report what you changed.** Do NOT change the `BlockBackend` trait or the
> `DiskInfo`/`VolumeInfo` types. The GPT-tempfile unit test (Step 3) and the loopback test (Step 5)
> are the acceptance criteria. If the trait can't be satisfied, STOP and report BLOCKED.

**Files:**
- Modify: `crates/block/src/sysfs.rs`
- Modify: `crates/block/src/lib.rs` (restore the module + re-export)
- Create: `crates/block/tests/loopback.rs`

- [ ] **Step 1: Implement `SysfsBlock`**

Replace `crates/block/src/sysfs.rs` with:

```rust
//! Pure-Rust read-only block discovery: `/sys/block` enumeration + GPT reading
//! (via the `gpt` crate) + filesystem-magic probing. Linux only.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::warn;

use crate::fsprobe::probe_fs;
use crate::{BlockBackend, BlockError, DiskInfo, Result, VolumeInfo};

/// Real backend reading from `/sys` and `/dev`. Roots are injectable for tests.
pub struct SysfsBlock {
    sys_root: PathBuf,
    dev_root: PathBuf,
}

impl Default for SysfsBlock {
    fn default() -> Self {
        Self::new()
    }
}

impl SysfsBlock {
    pub fn new() -> Self {
        Self {
            sys_root: PathBuf::from("/sys"),
            dev_root: PathBuf::from("/dev"),
        }
    }

    /// Construct with explicit roots (tests point these at fixtures/tempfiles).
    pub fn with_roots(sys_root: impl Into<PathBuf>, dev_root: impl Into<PathBuf>) -> Self {
        Self {
            sys_root: sys_root.into(),
            dev_root: dev_root.into(),
        }
    }

    fn read_partitions(&self, disk: &str) -> Result<Vec<PartEntry>> {
        let path = self.dev_root.join(disk);
        let device = path.to_string_lossy().to_string();
        let cfg = gpt::GptConfig::new().writable(false);
        let gpt_disk = cfg.open(&path).map_err(|e| BlockError::Gpt {
            device: device.clone(),
            message: e.to_string(),
        })?;
        // gpt 3.1: logical_block_size() returns &LogicalBlockSize (Copy); pass it
        // by value to bytes_len(), which takes the enum (not a u64).
        let lb = *gpt_disk.logical_block_size();
        let mut out = Vec::new();
        for (idx, part) in gpt_disk.partitions() {
            out.push(PartEntry {
                device: part_device(disk, *idx),
                uuid: part.part_guid.to_string(),
                label: part.name.clone(),
                type_guid: part.part_type_guid.guid.to_string(),
                size_bytes: part.bytes_len(lb).unwrap_or(0),
            });
        }
        Ok(out)
    }
}

struct PartEntry {
    device: String,
    uuid: String,
    label: String,
    type_guid: String,
    size_bytes: u64,
}

/// Partition device name: insert `p` before the number when the disk name ends
/// in a digit (nvme0n1 -> nvme0n1p1; sda -> sda1).
fn part_device(disk: &str, num: u32) -> String {
    let sep = if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    format!("{disk}{sep}{num}")
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_head(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut f = fs::File::open(path).map_err(|source| BlockError::Io {
        path: path.to_string_lossy().to_string(),
        source,
    })?;
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(k) => filled += k,
            Err(e) => {
                return Err(BlockError::Io {
                    path: path.to_string_lossy().to_string(),
                    source: e,
                })
            }
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

#[async_trait]
impl BlockBackend for SysfsBlock {
    async fn list_disks(&self) -> Result<Vec<DiskInfo>> {
        let block = self.sys_root.join("block");
        let entries = fs::read_dir(&block).map_err(|source| BlockError::Io {
            path: block.to_string_lossy().to_string(),
            source,
        })?;
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| BlockError::Io {
                path: block.to_string_lossy().to_string(),
                source,
            })?;
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip only pure memory-backed virtual devices. loop/dm/md are kept
            // deliberately: loop is needed by the loopback integration test, and a
            // device with no GPT is harmlessly skipped during list_volumes anyway.
            if name.starts_with("ram") || name.starts_with("zram") {
                continue;
            }
            let dir = block.join(&name);
            let size_sectors: u64 = read_trim(&dir.join("size"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            out.push(DiskInfo {
                name: name.clone(),
                path: self.dev_root.join(&name).to_string_lossy().to_string(),
                size_bytes: size_sectors.saturating_mul(512),
                model: read_trim(&dir.join("device/model")).unwrap_or_default(),
                serial: read_trim(&dir.join("device/serial")).unwrap_or_default(),
                rotational: read_trim(&dir.join("queue/rotational")).as_deref() == Some("1"),
                read_only: read_trim(&dir.join("ro")).as_deref() == Some("1"),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>> {
        let mut out = Vec::new();
        for disk in self.list_disks().await? {
            let parts = match self.read_partitions(&disk.name) {
                Ok(p) => p,
                Err(e) => {
                    warn!(disk = %disk.name, error = %e, "skipping disk: partition read failed");
                    continue;
                }
            };
            for p in parts {
                let dev_path = self.dev_root.join(&p.device);
                let probe = read_head(&dev_path, 8192).ok().and_then(|b| probe_fs(&b));
                out.push(VolumeInfo {
                    device: dev_path.to_string_lossy().to_string(),
                    disk: disk.name.clone(),
                    partition_uuid: p.uuid,
                    partition_label: p.label,
                    partition_type_guid: p.type_guid,
                    fs_type: probe.as_ref().map(|x| x.fs_type),
                    fs_label: probe.as_ref().and_then(|x| x.label.clone()),
                    fs_uuid: probe.as_ref().and_then(|x| x.uuid.clone()),
                    size_bytes: p.size_bytes,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn part_device_naming() {
        assert_eq!(part_device("sda", 1), "sda1");
        assert_eq!(part_device("nvme0n1", 2), "nvme0n1p2");
        assert_eq!(part_device("loop0", 1), "loop0p1");
    }

    #[tokio::test]
    async fn list_disks_parses_sysfs_fixture() {
        let dir = std::env::temp_dir().join(format!("mnd-sysfs-{}", std::process::id()));
        let sda = dir.join("block/sda");
        fs::create_dir_all(sda.join("device")).unwrap();
        fs::create_dir_all(sda.join("queue")).unwrap();
        let w = |p: PathBuf, v: &str| {
            let mut f = fs::File::create(p).unwrap();
            f.write_all(v.as_bytes()).unwrap();
        };
        w(sda.join("size"), "2048\n");
        w(sda.join("ro"), "0\n");
        w(sda.join("queue/rotational"), "1\n");
        w(sda.join("device/model"), "TEST MODEL\n");
        w(sda.join("device/serial"), "SER123\n");
        // A ram device that must be filtered out.
        fs::create_dir_all(dir.join("block/ram0")).unwrap();
        w(dir.join("block/ram0/size"), "100\n");

        let be = SysfsBlock::with_roots(&dir, "/dev");
        let disks = be.list_disks().await.unwrap();
        fs::remove_dir_all(&dir).ok();

        assert_eq!(disks.len(), 1, "ram0 filtered");
        let d = &disks[0];
        assert_eq!(d.name, "sda");
        assert_eq!(d.size_bytes, 2048 * 512);
        assert!(d.rotational);
        assert!(!d.read_only);
        assert_eq!(d.model, "TEST MODEL");
        assert_eq!(d.serial, "SER123");
    }

    // Reads a GPT written into a tempfile (no kernel partitions needed to read
    // the partition table itself).
    #[test]
    fn read_partitions_from_gpt_tempfile() {
        let dir = std::env::temp_dir().join(format!("mnd-gpt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let img = dir.join("sdz");

        // Create a 16 MiB image and write a GPT with two partitions.
        {
            let f = fs::File::create(&img).unwrap();
            f.set_len(16 * 1024 * 1024).unwrap();
        }
        let mut gdisk = gpt::GptConfig::new()
            .writable(true)
            .initialized(false)
            .open(&img)
            .unwrap();
        gdisk
            .update_partitions(std::collections::BTreeMap::new())
            .unwrap();
        // `Type` is Clone-not-Copy, so pass the const directly to each call
        // rather than binding (which would move on first use).
        gdisk
            .add_partition("EFI", 1024 * 1024, gpt::partition_types::EFI, 0, None)
            .unwrap();
        gdisk
            .add_partition("STATE", 1024 * 1024, gpt::partition_types::EFI, 0, None)
            .unwrap();
        gdisk.write().unwrap();

        let be = SysfsBlock::with_roots("/sys", &dir);
        let parts = be.read_partitions("sdz").unwrap();
        fs::remove_dir_all(&dir).ok();

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].device, "sdz1");
        assert_eq!(parts[1].device, "sdz2");
        assert!(parts.iter().any(|p| p.label == "EFI"));
        assert!(parts.iter().any(|p| p.label == "STATE"));
    }
}
```

> The GPT-tempfile test (`read_partitions_from_gpt_tempfile`) doubles as the spike harness: it both
> writes and reads a GPT through the `gpt` crate, so getting it to compile + pass confirms the API.
> If `add_partition`/`update_partitions`/`write`/`logical_block_size`/`part_type_guid.guid`/
> `bytes_len` differ in the resolved version, adapt them (and the `read_partitions` internals
> identically) and report the changes. Keep `DiskInfo`/`VolumeInfo`/the trait unchanged.

- [ ] **Step 2: Restore the module + re-export**

In `crates/block/src/lib.rs`, restore:

```rust
pub mod fake;
pub mod fsprobe;
#[cfg(target_os = "linux")]
pub mod sysfs;
```

```rust
pub use fake::FakeBlockBackend;
pub use fsprobe::{probe_fs, FsProbe};
#[cfg(target_os = "linux")]
pub use sysfs::SysfsBlock;
```

- [ ] **Step 3: Build + unit tests (spike gate)**

Run: `cargo build -p machined-block`
Expected: PASS. If `gpt` API differs, adapt per the SPIKE NOTE until it builds; record changes.

Run: `cargo test -p machined-block`
Expected: PASS — fsprobe (5) + fake (1) + `part_device_naming` + `list_disks_parses_sysfs_fixture` + `read_partitions_from_gpt_tempfile`.

Run: `cargo clippy -p machined-block --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Write the loopback integration test**

Create `crates/block/tests/loopback.rs`:

```rust
//! Privileged integration test: write a GPT into a sparse file, attach it as a
//! loop device, and discover it through the real SysfsBlock. Ignored by default;
//! run with: sudo -E cargo test -p machined-block --test loopback -- --ignored
//! Requires root (losetup) + CAP_SYS_ADMIN.

#![cfg(target_os = "linux")]

use std::process::Command;

use machined_block::{BlockBackend, SysfsBlock};

#[tokio::test]
#[ignore = "requires root + losetup"]
async fn discovers_loopback_disk_partitions() {
    let img = std::env::temp_dir().join("mnd-loop.img");
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
    }
    // Write a GPT with one partition using the gpt crate.
    let mut g = gpt::GptConfig::new()
        .writable(true)
        .initialized(false)
        .open(&img)
        .unwrap();
    g.update_partitions(std::collections::BTreeMap::new()).unwrap();
    g.add_partition("STATE", 8 * 1024 * 1024, gpt::partition_types::LINUX_FS, 0, None)
        .unwrap();
    g.write().unwrap();

    // Attach as a loop device with partition scanning (-P).
    let out = Command::new("losetup")
        .args(["-fP", "--show", img.to_str().unwrap()])
        .output()
        .expect("losetup");
    assert!(out.status.success(), "losetup failed");
    let loopdev = String::from_utf8(out.stdout).unwrap().trim().to_string(); // e.g. /dev/loop3
    let loopname = loopdev.trim_start_matches("/dev/").to_string();

    let be = SysfsBlock::new();
    let disks = be.list_disks().await.unwrap();
    assert!(disks.iter().any(|d| d.name == loopname), "loop disk discovered");

    let vols = be.list_volumes().await.unwrap();
    let found = vols.iter().any(|v| v.disk == loopname && v.partition_label == "STATE");

    // Detach before asserting so we always clean up.
    let _ = Command::new("losetup").args(["-d", &loopdev]).status();
    std::fs::remove_file(&img).ok();

    assert!(found, "STATE partition discovered on loop device");
}
```

> `gpt` is needed in the integration test too; add `gpt.workspace = true` to `crates/block/Cargo.toml`
> `[dev-dependencies]` (alongside `tokio`) if the test target doesn't already see it through the
> linux target dep. Note this in your report if you added it.

- [ ] **Step 5: Run the default suite (loopback ignored) + best-effort privileged run**

Run: `cargo test -p machined-block`
Expected: unit tests pass; `discovers_loopback_disk_partitions` listed as ignored.

If the environment allows: `sudo -E cargo test -p machined-block --test loopback -- --ignored --nocapture` → PASS. If it cannot run privileged tests, note that explicitly (do not claim a pass you didn't run).

- [ ] **Step 6: Full workspace gate + commit**

Run: `cargo build --workspace` → PASS.
Run: `cargo clippy --all-targets --all-features -- -D warnings` → clean.
Run: `cargo test --workspace` → all pass (loopback ignored).
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add crates/block Cargo.toml Cargo.lock
git commit -m "feat(block): SysfsBlock real backend + loopback integration test"
```

---

## Task 5: `DiskDiscoveryController` + machined wiring + e2e

**Files:**
- Create: `crates/controllers/src/block/mod.rs`
- Create: `crates/controllers/src/block/discovery.rs`
- Modify: `crates/controllers/src/lib.rs`
- Modify: `crates/controllers/Cargo.toml` (add machined-block dep)
- Modify: `crates/machined/Cargo.toml` (add machined-block dep)
- Modify: `crates/machined/src/main.rs`
- Create: `crates/machined/tests/block.rs`

- [ ] **Step 1: Add the block dep to controllers**

In `crates/controllers/Cargo.toml` `[dependencies]`, add:

```toml
machined-block.workspace = true
```

- [ ] **Step 2: Write the DiskDiscoveryController**

Create `crates/controllers/src/block/discovery.rs`:

```rust
//! Enumerates block devices via the `BlockBackend` and publishes `DiskStatus`
//! and `DiscoveredVolume` resources, GC'ing devices that have disappeared.

use std::sync::Arc;

use async_trait::async_trait;
use machined_block::BlockBackend;
use machined_resources::{
    DiscoveredVolume, DiskStatus, Resource, ResourceObject, ResourceType,
};
use machined_runtime_core::{reconcile_owned, Controller, Input, Output, OutputKind, ReconcileCtx};

use super::{ctl, NS};

const OWNER: &str = "disk-discovery";

pub struct DiskDiscoveryController {
    backend: Arc<dyn BlockBackend>,
}

impl DiskDiscoveryController {
    pub fn new(backend: Arc<dyn BlockBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl Controller for DiskDiscoveryController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        // Boot-time run-once: the startup reconcile enumerates. Hotplug refresh
        // is a later milestone.
        Vec::new()
    }

    fn outputs(&self) -> Vec<Output> {
        [ResourceType::DiskStatus, ResourceType::DiscoveredVolume]
            .into_iter()
            .map(|typ| Output {
                typ,
                kind: OutputKind::Exclusive,
            })
            .collect()
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let disks = self.backend.list_disks().await.map_err(ctl)?;
        let disk_objs = disks
            .into_iter()
            .map(|d| {
                ResourceObject::new(
                    NS,
                    &d.name,
                    Resource::DiskStatus(DiskStatus {
                        name: d.name.clone(),
                        path: d.path,
                        size_bytes: d.size_bytes,
                        model: d.model,
                        serial: d.serial,
                        rotational: d.rotational,
                        read_only: d.read_only,
                    }),
                )
            })
            .collect();
        reconcile_owned(&ctx.state, OWNER, NS, ResourceType::DiskStatus, disk_objs)?;

        let volumes = self.backend.list_volumes().await.map_err(ctl)?;
        let vol_objs = volumes
            .into_iter()
            .map(|v| {
                let id = leaf(&v.device);
                ResourceObject::new(
                    NS,
                    &id,
                    Resource::DiscoveredVolume(DiscoveredVolume {
                        device: v.device.clone(),
                        disk: v.disk,
                        partition_uuid: v.partition_uuid,
                        partition_label: v.partition_label,
                        partition_type_guid: v.partition_type_guid,
                        fs_type: v.fs_type.map(|t| t.as_str().to_string()),
                        fs_label: v.fs_label,
                        fs_uuid: v.fs_uuid,
                        size_bytes: v.size_bytes,
                    }),
                )
            })
            .collect();
        reconcile_owned(
            &ctx.state,
            OWNER,
            NS,
            ResourceType::DiscoveredVolume,
            vol_objs,
        )?;
        Ok(())
    }
}

/// Resource id for a volume: the device leaf (`/dev/sda1` -> `sda1`).
fn leaf(device: &str) -> String {
    device.rsplit('/').next().unwrap_or(device).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_block::{DiskInfo, FakeBlockBackend, FsType, VolumeInfo};
    use machined_runtime_core::{ReconcileCtx, State};

    fn disk(name: &str) -> DiskInfo {
        DiskInfo {
            name: name.into(),
            path: format!("/dev/{name}"),
            size_bytes: 1 << 30,
            model: "M".into(),
            serial: "S".into(),
            rotational: false,
            read_only: false,
        }
    }

    fn vol(disk: &str, dev: &str) -> VolumeInfo {
        VolumeInfo {
            device: format!("/dev/{dev}"),
            disk: disk.into(),
            partition_uuid: "u".into(),
            partition_label: "STATE".into(),
            partition_type_guid: "g".into(),
            fs_type: Some(FsType::Ext4),
            fs_label: None,
            fs_uuid: None,
            size_bytes: 1 << 20,
        }
    }

    #[tokio::test]
    async fn publishes_disks_and_volumes() {
        let backend = Arc::new(
            FakeBlockBackend::new()
                .with_disk(disk("sda"))
                .with_volume(vol("sda", "sda1")),
        );
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = DiskDiscoveryController::new(backend);
        c.reconcile(&ctx).await.unwrap();

        assert_eq!(state.list(NS, ResourceType::DiskStatus).len(), 1);
        let vols = state.list(NS, ResourceType::DiscoveredVolume);
        assert_eq!(vols.len(), 1);
        match &vols[0].spec {
            Resource::DiscoveredVolume(v) => assert_eq!(v.fs_type.as_deref(), Some("ext4")),
            _ => panic!("wrong type"),
        }
    }

    #[tokio::test]
    async fn gcs_disappeared_devices() {
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        // First pass sees sda.
        let mut c1 = DiskDiscoveryController::new(Arc::new(
            FakeBlockBackend::new().with_disk(disk("sda")),
        ));
        c1.reconcile(&ctx).await.unwrap();
        assert_eq!(state.list(NS, ResourceType::DiskStatus).len(), 1);

        // Second pass: sda gone → GC'd (no finalizers).
        let mut c2 = DiskDiscoveryController::new(Arc::new(FakeBlockBackend::new()));
        c2.reconcile(&ctx).await.unwrap();
        assert_eq!(state.list(NS, ResourceType::DiskStatus).len(), 0);
    }
}
```

- [ ] **Step 3: Write the block module helpers**

Create `crates/controllers/src/block/mod.rs`:

```rust
//! Block controllers. M2b-1: read-only discovery.

pub mod discovery;

pub use discovery::DiskDiscoveryController;

use std::fmt::Display;

use machined_runtime_core::Error;

/// Namespace for block resources.
pub const NS: &str = "block";

/// Map a backend error into a runtime-core controller error.
pub(crate) fn ctl<E: Display>(e: E) -> Error {
    Error::Controller(e.to_string())
}
```

In `crates/controllers/src/lib.rs`, add the module:

```rust
//! machined-rs controllers.

pub mod block;
pub mod network;
```

- [ ] **Step 4: Test the controller**

Run: `cargo test -p machined-controllers block` → `publishes_disks_and_volumes` + `gcs_disappeared_devices` pass.
Run: `cargo clippy -p machined-controllers --all-targets -- -D warnings` → clean.

- [ ] **Step 5: Wire into machined**

In `crates/machined/Cargo.toml` `[dependencies]`, add:

```toml
machined-block.workspace = true
```

In `crates/machined/src/main.rs`, add the import:

```rust
use machined_controllers::block::DiskDiscoveryController;
```

Add a backend builder mirroring `build_network_backend`:

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

In `run_daemon`, after the network controllers are registered (and before the runtime is spawned), register the discovery controller:

```rust
    runtime.register(Box::new(DiskDiscoveryController::new(build_block_backend())));
```

- [ ] **Step 6: Write the e2e discovery test**

Create `crates/machined/tests/block.rs`:

```rust
//! End-to-end: the discovery controller on the real Runtime against a fake
//! block backend populates the store with disk + volume status. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_block::{DiskInfo, FakeBlockBackend, FsType, VolumeInfo};
use machined_controllers::block::{DiskDiscoveryController, NS};
use machined_resources::{Resource, ResourceType};
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn discovery_populates_store() {
    let backend = Arc::new(
        FakeBlockBackend::new()
            .with_disk(DiskInfo {
                name: "vda".into(),
                path: "/dev/vda".into(),
                size_bytes: 8 << 30,
                model: "VIRT".into(),
                serial: "V1".into(),
                rotational: false,
                read_only: false,
            })
            .with_volume(VolumeInfo {
                device: "/dev/vda1".into(),
                disk: "vda".into(),
                partition_uuid: "u".into(),
                partition_label: "STATE".into(),
                partition_type_guid: "g".into(),
                fs_type: Some(FsType::Ext4),
                fs_label: None,
                fs_uuid: None,
                size_bytes: 1 << 30,
            }),
    );

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(DiskDiscoveryController::new(backend)));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut ok = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let disks = state.list(NS, ResourceType::DiskStatus);
        let vols = state.list(NS, ResourceType::DiscoveredVolume);
        if disks.len() == 1 && vols.len() == 1 {
            if let Resource::DiscoveredVolume(v) = &vols[0].spec {
                if v.fs_type.as_deref() == Some("ext4") {
                    ok = true;
                    break;
                }
            }
        }
    }
    assert!(ok, "discovery did not populate disk + volume status");

    shutdown.cancel();
    let _ = handle.await;
}
```

- [ ] **Step 7: Full gate + commit**

Run: `cargo build --workspace` → PASS.
Run: `cargo run -p machined -- version` → `machined 0.1.0`.
Run: `cargo test -p machined --test block` → PASS.
Run: `make pre-commit` → fmt + clippy -D warnings + full workspace test all green (loopback ignored).

```bash
git add crates/controllers crates/machined Cargo.lock
git commit -m "feat(machined): register DiskDiscoveryController + e2e block test"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** `block` crate with `BlockBackend` + `SysfsBlock` (pure-Rust /sys + gpt + magic probe) + fake (Tasks 2–4) ✓; `DiskStatus`/`DiscoveredVolume` resources (Task 1) ✓; ext4/vfat/xfs/swap probing (Task 3) ✓; `DiskDiscoveryController` via `reconcile_owned` with GC (Task 5) ✓; machined wiring + root-free e2e (Task 5) ✓; gated loopback integration test (Task 4) ✓.
- **Deliberate M2b-1 limits (per spec):** read-only only — no partition/format/mount (M2b-2); `fs_label`/`fs_uuid` populated for ext4 only (others `None`); boot-time run-once (no hotplug); no config. `gpt` spike is isolated to `SysfsBlock::read_partitions` — the trait/types/controller are deterministic and fully tested without a real device.
- **Type consistency:** `BlockBackend`/`DiskInfo`/`VolumeInfo`/`FsType` (block) ↔ `DiskStatus`/`DiscoveredVolume` (resources); `reconcile_owned`/`Controller`/`Output`/`OutputKind` (runtime-core); the controller maps `FsType` → string for the resource. Discovery uses `reconcile_owned` (not `publish_status`) so the discovered set is GC'd — the deliberate reuse from the spec.
- **Placeholder scan:** none; Task 4's `gpt` code is real best-effort with an explicit spike protocol, not a placeholder.
