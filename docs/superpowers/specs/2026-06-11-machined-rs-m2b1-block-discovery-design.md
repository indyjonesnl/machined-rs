# machined-rs M2b-1 — Block Discovery, Design

**Date:** 2026-06-11
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (this refines milestone M2's block portion)
**Builds on:** M0 (`runtime-core`) + M1 (boot) + M2a (network controllers / owner-cascade), merged to `main`.

## 1. Overview

M2b-1 makes a booted node discover its block devices: it enumerates disks, reads their GPT
partition tables, probes partition filesystems, and publishes the result as observed-state resources
in the store via the M0 reconcile runtime. It is **entirely read-only** — no partitioning, no
formatting, no mounting. It is the safe first half of the block subsystem.

The original roadmap's M2b (block) decomposes into:
- **M2b-1 — Discovery** (this spec): enumerate disks, read partitions, probe filesystems → status.
- **M2b-2 — Provisioning + mount**: partition (GPT) + format (mkfs) + mount BOOT/STATE/EPHEMERAL, with
  the strict destructive-op safety posture. A later cycle.

## 2. Goals / Non-goals

### Goals
- Add a `block` crate: a `BlockBackend` trait with a pure-Rust `SysfsBlock` implementation and a fake.
- Enumerate block devices from `/sys/block` (size, model, serial, rotational, read-only).
- Read GPT partition tables via the `gpt` crate (partition device, UUID, type GUID, label, size).
- Probe partition filesystem type by magic bytes for **ext4, vfat, xfs, swap** (unknown → `None`).
- Publish `DiskStatus` + `DiscoveredVolume` resources via `reconcile_owned`, so removed devices are
  garbage-collected.
- Wire a `DiskDiscoveryController` into `machined` so a booted node populates the store with its
  block topology.

### Non-goals (deferred)
- **All destructive operations** — partitioning, formatting, wiping (M2b-2).
- **Mounting** filesystems (M2b-2).
- The `install.disk` config + strict wipe-guard safety logic (M2b-2, where provisioning uses them).
- Hotplug / udev-driven refresh (discovery is boot-time run-once for now).
- LVM, encryption, swap activation, zswap, RAID, multipath.
- Filesystems beyond the four probed; anything requiring a full superblock parse (we read only the
  magic + label/uuid where trivially available).

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `resources` | + observed `DiskStatus` and `DiscoveredVolume` specs + new `ResourceType` variants. Status-only. |
| `block` (new leaf crate) | `BlockBackend` trait + `DiskInfo`/`VolumeInfo`/`FsType` types + `SysfsBlock` (pure-Rust) + `FakeBlockBackend`. Depends only on `resources`/`common` + the `gpt` crate (Linux). Mirrors the `netlink` trait/real/fake pattern. |
| `controllers` | + `block` module: `DiskDiscoveryController`. |
| `machined` | register `DiskDiscoveryController` (real `SysfsBlock` on Linux, fake otherwise). |

Dependency direction: `block` is a leaf (like `netlink`). `controllers` depends on it. `machined`
wires it. No crate reaches sideways; the `gpt` crate is confined to `block` behind
`cfg(target_os = "linux")`.

### 3.2 Block types (in `block`)

- `FsType { Ext4, Vfat, Xfs, Swap }` (probed set; unknown filesystems map to `None` in `VolumeInfo`).
- `DiskInfo { name /* "sda","nvme0n1" */, path /* "/dev/sda" */, size_bytes, model, serial, rotational, read_only }`.
- `VolumeInfo { device /* "/dev/sda1" */, disk /* parent "sda" */, partition_uuid, partition_label, partition_type_guid, fs_type: Option<FsType>, fs_label: Option<String>, fs_uuid: Option<String>, size_bytes }`.

These info types are shaped so M2b-2 provisioning can reuse them (a planned partition is a
`VolumeInfo`-shaped desired spec).

### 3.3 `BlockBackend` trait

```text
trait BlockBackend (async, Send + Sync):
    list_disks()   -> Vec<DiskInfo>     // enumerate block devices
    list_volumes() -> Vec<VolumeInfo>   // partitions + probed filesystems across all disks
```

The trait is defined with discovery methods only in M2b-1. M2b-2 grows it with provisioning
(`partition`/`format`/`mount`); no stubbed methods are added now.

- **`SysfsBlock`** (Linux, `cfg`-gated): takes an injectable sysfs root (default `/sys`) and dev root
  (default `/dev`) so it is testable against fixtures. `list_disks` reads `/sys/block/<name>`
  (`size` sectors × 512, `device/model`, `device/serial`, `queue/rotational`, `ro`); skips virtual
  devices (loop, ram, dm) by name prefix unless they back a real disk. `list_volumes` opens each disk
  read-only via the `gpt` crate to read partitions, then probes each partition's filesystem by
  reading magic bytes from the device.
- **`FakeBlockBackend`**: holds in-memory `Vec<DiskInfo>` + `Vec<VolumeInfo>`, returned by the trait
  methods; constructed via builders for tests.

### 3.4 `DiskStatus` / `DiscoveredVolume` resources

Observed-state resources mirroring `DiskInfo`/`VolumeInfo`. Namespace `block`. Deterministic ids:
`DiskStatus` id = disk name (`sda`); `DiscoveredVolume` id = device leaf (`sda1`).

### 3.5 `DiskDiscoveryController`

- Inputs: none (boot-time run-once; reconciles at startup).
- Outputs: `DiskStatus`, `DiscoveredVolume` (Exclusive, owned).
- Reconcile: `backend.list_disks()` → `reconcile_owned(... DiskStatus, disks)`;
  `backend.list_volumes()` → `reconcile_owned(... DiscoveredVolume, volumes)`. Using
  `reconcile_owned` means a device present last pass but gone now is torn down and (no finalizers)
  destroyed — the store always reflects current topology.

### 3.6 Wiring

`machined::run_daemon` builds the backend (`SysfsBlock::new()` on Linux, `FakeBlockBackend` otherwise)
and registers `DiskDiscoveryController` into the `Runtime` alongside the network controllers.

## 4. Error handling & observability

- One `Error` enum in `block` via `thiserror`; discovery failures (an unreadable disk, a bad GPT) are
  logged via `tracing` and skipped — discovery of one disk must not abort enumeration of the others.
- A controller-level `ctl` mapping (as in the network controllers) turns backend errors into
  `runtime_core::Error::Controller` for the reconcile path; a failed list is retried next reconcile.

## 5. Testing strategy

- **Unit (root-free):**
  - `SysfsBlock::list_disks` against a fixture `/sys/block` tree (injected sysfs root): asserts
    parsing of size/model/serial/rotational/ro and virtual-device filtering.
  - GPT reading: write a small GPT into a tempfile with the `gpt` crate in-test, then read it back
    through the partition-reading path; assert partition device/uuid/label.
  - Filesystem-magic probing: small fixture blobs with ext4/vfat/xfs/swap magic at the right offsets
    → asserts the right `FsType`; a random blob → `None`.
  - `DiskDiscoveryController` against `FakeBlockBackend`: publishes `DiskStatus`/`DiscoveredVolume`;
    a second pass with a device removed GC's its resources.
- **Integration (privileged, gated):** a test that `losetup`s a sparse file containing a GPT and reads
  it through the real `SysfsBlock`, asserting the partitions, gated behind root/CI (like the netns
  test). `#[ignore]` by default so `cargo test --workspace` stays root-free.
- **CI:** `make pre-commit` parity for the unit tier; the privileged test runs separately.

## 6. Key risks

- **`gpt` crate API/ergonomics** — spike reading a generated GPT early; if the crate can't read a
  partition field we need, fall back to a minimal hand-rolled GPT header/entry parse (GPT layout is
  stable and simple) inside `SysfsBlock`, without changing the trait.
- **Filesystem-magic offsets** — get the four magics/offsets right (ext4 `0xEF53` at 0x438; vfat
  signature in the boot sector; xfs `XFSB` at 0; swap `SWAPSPACE2`/`SWAP-SPACE` at end of the first
  page). Cover each with a fixture test.
- **sysfs parsing variance** — different device classes expose different attributes (NVMe vs SATA vs
  virtio); missing attributes must degrade to empty/None, never panic. The injectable-root fixture
  tests pin the parsing.
- **Reading raw devices needs privilege** — `list_volumes` opening `/dev/sdX` for GPT needs root, so
  it is covered by the gated loopback test, not unit tests; the unit tier exercises the GPT/probe
  logic against tempfiles instead.
