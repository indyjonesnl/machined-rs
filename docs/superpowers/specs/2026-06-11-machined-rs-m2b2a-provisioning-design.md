# machined-rs M2b-2a — Block Provisioning, Design

**Date:** 2026-06-11
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (refines milestone M2b's provisioning portion)
**Builds on:** M0/M1/M2a + M2b-1 (block discovery: `BlockBackend`, `DiscoveredVolume`), merged to `main`.

## 1. Overview

M2b-2a makes a node provision its install disk: it lays out a fixed GPT partition scheme and creates
filesystems, **guarded** so it only ever touches the config-named install disk and never destroys
existing data without explicit consent. It is the **destructive** half of the block subsystem; the
safety guard is its most important component.

M2b (block) decomposes into:
- **M2b-1 — Discovery** (done): read-only enumeration of disks/partitions/filesystems.
- **M2b-2a — Provisioning** (this spec): wipe/partition/format the install disk, guarded.
- **M2b-2b — Mount + volume lifecycle** (later): mount EFI/STATE/EPHEMERAL via the platform, publish
  mount status, teardown.

## 2. Goals / Non-goals

### Goals
- Extend `BlockBackend` with `wipe`, `create_partitions`, `format` (the destructive operations).
  `SysfsBlock` implements them (GPT write via the `gpt` crate; `mkfs.ext4`/`mkfs.vfat` shell-out);
  `FakeBlockBackend` simulates them in memory.
- Add an `install { disk, wipe }` machine-config section.
- Add a `VolumeStatus` resource for the managed volumes (EFI/STATE/EPHEMERAL).
- Provide a **pure `plan_provisioning` decision function** — the safety guard — that classifies the
  install disk's current state into `Skip` / `Provision` / `RefuseForeign`, exhaustively testable
  without any device.
- Add a `VolumeProvisionerController` that runs the guard and, only on `Provision`, executes the
  destructive operations, then publishes `VolumeStatus`.
- Lay out the fixed scheme on the install disk: EFI (vfat, 512 MiB), STATE (ext4, 1 GiB),
  EPHEMERAL (ext4, remaining space).

### Non-goals (deferred)
- **Mounting** the provisioned volumes and the mount lifecycle (M2b-2b).
- Config-declared/custom volume layouts (the scheme is fixed).
- Encryption (LUKS), LVM, swap activation, RAID, resizing an existing layout, multi-disk.
- Re-partitioning to *change* an existing valid layout (only blank/foreign-with-wipe → provision).
- Bootloader installation / writing boot assets to EFI (the partition is created + formatted only).

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `block` | + `PartitionPlan` type; + `BlockBackend::{wipe, create_partitions, format}`. `SysfsBlock` real impl (gpt-crate write + `mkfs` shell-out); `FakeBlockBackend` simulates (mutates its in-memory disks/volumes so a follow-up `list_volumes` reflects the new layout). |
| `config` | + `InstallSection { disk: String, wipe: bool /* default false */ }` on `MachineSection`. |
| `resources` | + `VolumeStatus { name, device, fs, label, phase }` + `VolumePhase` (Provisioned / Failed). |
| `controllers` | + `block::provision` module: pure `plan_provisioning(...) -> ProvisionDecision` + `VolumeProvisionerController`. |
| `machined` | register `VolumeProvisionerController`. |

`block` stays a leaf; `gpt` write + `mkfs` shell-out stay confined to `SysfsBlock` (Linux). The
controller depends on `block` + `config` + `resources` + `runtime-core` as the network/discovery
controllers do.

### 3.2 `BlockBackend` provisioning methods

```text
PartitionPlan { label: String, type_guid: PartType, fs: FsType, size_bytes: u64 /* 0 = rest */ }
PartType = EfiSystem | LinuxFilesystem   // the GPT type GUIDs we use

trait BlockBackend (extended):
    async fn wipe(&self, disk: &str) -> Result<()>
        // destroy the partition table (zap primary+backup GPT headers / first+last sectors)
    async fn create_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>>
        // write a fresh GPT with the planned partitions; return the partition device paths in order
    async fn format(&self, device: &str, fs: FsType, label: &str) -> Result<()>
        // create the filesystem (mkfs.ext4 / mkfs.vfat) with the given label
```

- **`SysfsBlock`**: `wipe` zeroes the GPT regions; `create_partitions` opens the disk writable via the
  `gpt` crate (the write path was already exercised by M2b-1's tempfile test) and triggers a kernel
  partition re-read; `format` shells out to `mkfs.ext4 -L <label>` / `mkfs.vfat -n <label>` via
  `tokio::process`.
- **`FakeBlockBackend`**: `wipe` clears the disk's volumes; `create_partitions` appends `VolumeInfo`
  entries (deterministic device names, sizes, labels, type GUIDs) and returns their paths; `format`
  sets the matching volume's `fs_type`/`fs_label`. So a controller test can `provision → list_volumes`
  and see the result — exercising the whole decide→act→verify loop without a real device.

### 3.3 Fixed partition layout

| order | label | fs | size | GPT type |
|---|---|---|---|---|
| 1 | `EFI` | vfat | 512 MiB | EFI System Partition |
| 2 | `STATE` | ext4 | 1 GiB | Linux filesystem |
| 3 | `EPHEMERAL` | ext4 | remaining | Linux filesystem |

Defined as a constant `fn fixed_layout() -> Vec<PartitionPlan>` in the controller crate.

### 3.4 The safety guard — `plan_provisioning` (pure)

```text
plan_provisioning(install_disk: &str, wipe: bool, discovered: &[DiscoveredVolume]) -> ProvisionDecision

ProvisionDecision =
    Skip                  // the disk already carries our exact layout (EFI + STATE + EPHEMERAL,
                          //   and nothing else) — idempotent no-op
  | Provision             // wipe == true and the disk is not already ours — lay out fresh
  | RefuseForeign         // wipe == false and the disk is not already ours — refuse; do not touch
```

Rules (evaluated against only the volumes whose `disk` == `install_disk`, matched by EXACT
parent-disk name):
- **Exactly our labels present** (EFI + STATE + EPHEMERAL, and no others) → `Skip`.
- **Anything else** (blank-looking / partial / foreign):
  - `wipe == true` → `Provision`.
  - `wipe == false` → `RefuseForeign`.

**CONSERVATIVE SAFETY (revised after review):** `wipe == false` is **never destructive** — it yields
only `Skip` or `RefuseForeign`. The earlier "blank disk → Provision without wipe" rule was **unsafe**:
discovery is GPT-only and best-effort, so an MBR disk, an unreadable disk, or a disk discovery simply
hasn't scanned yet all *look* blank (no `DiscoveredVolume`) — auto-provisioning them would destroy
real foreign data under `wipe:false`. Therefore provisioning **any** disk not already carrying our
exact layout requires explicit `install.wipe: true` (like Talos's explicit install). The absence of a
GPT layout is treated as "unknown, possibly foreign", not "blank".

This function is **pure** (no I/O) and is the single source of the destructive decision. It is tested
exhaustively; the controller's only destructive code runs strictly inside the `Provision` branch.

### 3.5 `VolumeProvisionerController`

- Inputs: `DiscoveredVolume` + `DiskStatus` (both from M2b-1 discovery), declared `Weak` so the
  provisioner re-evaluates when discovery publishes.
- Outputs: `VolumeStatus` (Exclusive, owned via `reconcile_owned`).
- Reconcile:
  1. If `install.disk` is empty/unset → nothing to do.
  2. **Discovery barrier:** if the install disk's `DiskStatus` is **not** present in the store →
     no-op (return `Ok`, quietly). Discovery has not yet scanned this disk (or it is absent), and the
     empty `DiscoveredVolume` set must **not** be read as "blank". This closes the boot race where the
     provisioner's initial reconcile could run before discovery populates the store.
  3. `decision = plan_provisioning(install.disk, install.wipe, discovered)`.
  4. `RefuseForeign` → `error!` with the disk; return an error (a deliberate halt; do **not** touch).
  5. `Skip` → publish `VolumeStatus(Provisioned)` for EFI/STATE/EPHEMERAL from the discovered volumes.
  6. `Provision` → `wipe` (if the disk had any discovered volumes) → `create_partitions(fixed_layout())`
     → `format` each → publish `VolumeStatus(Provisioned)` per volume. Idempotent: once our labels are
     present the guard returns `Skip`, so a retry after partial failure re-converges.

### 3.6 Discovery barrier (the boot-race fix)

The provisioner must not act on an empty store before discovery has run. Two changes provide a clean
happens-after barrier using the existing `DiskStatus` resource:

1. **`DiskDiscoveryController` publishes `DiscoveredVolume` BEFORE `DiskStatus`** within its reconcile
   (both `await`s complete first, then the two `reconcile_owned` calls run back-to-back with no
   `await` between them). So **`DiskStatus` present ⇒ that scan's `DiscoveredVolume`s are already in
   the store.**
2. The provisioner declares `DiskStatus` as a `Weak` input and **gates on the install disk's
   `DiskStatus` being present** (step 2 above). It therefore only ever evaluates `plan_provisioning`
   against a discovered view that reflects a completed scan — never an empty pre-discovery store.

### 3.7 Wiring

`machined::run_daemon` registers `DiskDiscoveryController` then `VolumeProvisionerController` into the
runtime. Order no longer carries a correctness requirement (the barrier handles ordering); the
controllers run as independent reconcile loops and the provisioner waits for the `DiskStatus` barrier
regardless of spawn order.

## 4. Error handling & observability

- New `BlockError` variants for wipe/partition/format failures (with device + message).
- `RefuseForeign` is surfaced as a controller error and logged prominently — it is a deliberate halt,
  not a silent skip.
- Every destructive operation logs the disk/device and the action before performing it.
- `VolumeStatus` makes the provisioned/failed state observable through the store (and the future API).

## 5. Testing strategy

- **Unit (root-free):**
  - `plan_provisioning`: blank → Provision; exact-layout → Skip; foreign+wipe=false → RefuseForeign;
    foreign+wipe=true → Provision; volumes on a *different* disk ignored; partial-our-layout → treated
    as foreign (RefuseForeign unless wipe). Exhaustive — this is the safety-critical function.
  - `VolumeProvisionerController` against `FakeBlockBackend`: blank disk → provisions + formats +
    publishes 3 `VolumeStatus(Provisioned)`; second reconcile → `Skip` (idempotent, no re-partition);
    foreign+wipe=false → no destructive call made (assert the fake recorded no wipe/create/format) +
    error; foreign+wipe=true → wipes + reprovisions.
  - `FakeBlockBackend` provisioning simulation: `create_partitions` + `format` then `list_volumes`
    reflects the new layout.
- **Integration (privileged, gated):** a loopback test — attach a sparse file as a loop device, run
  the real `SysfsBlock` `wipe`+`create_partitions`+`format`, then re-discover and assert the EFI/STATE/
  EPHEMERAL partitions exist with the right labels/filesystems. Gated behind root (`losetup` +
  `mkfs`), `#[ignore]` by default.
- **CI:** `make pre-commit` for the unit tier; the privileged loopback test runs separately.

## 6. Key risks

- **Destructive correctness** — the guard is the protection; cover `plan_provisioning` exhaustively
  and assert in controller tests that the `RefuseForeign`/`Skip` paths make **zero** destructive
  backend calls (the fake records calls so this is checkable).
- **gpt-crate write + kernel re-read** — writing a GPT is proven (M2b-1 tempfile test); the new bit is
  making the kernel re-read so `/dev/sdaN` nodes appear. Spike inside the loopback test (`losetup -P`
  + the gpt write, or `BLKRRPART`); if the crate can't trigger the re-read, shell `partprobe` or
  issue the ioctl from `SysfsBlock` — without changing the trait.
- **`mkfs` availability** — `format` shells out; the real path is covered only by the gated loopback
  test (needs e2fsprogs + dosfstools). The fake covers the controller logic.
- **Idempotency** — re-running provisioning must converge: once labels are present the guard returns
  `Skip`, so a retry after a partial failure re-partitions/reformats safely and then settles.
