# machined-rs M2b-2b — Volume Mount, Design

**Date:** 2026-06-11
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (completes milestone M2b's block subsystem)
**Builds on:** M2b-1 (discovery) + M2b-2a (provisioning → `VolumeStatus`), merged to `main`.

## 1. Overview

M2b-2b mounts the provisioned system volumes at their fixed mountpoints, completing the block
pipeline: **discover (M2b-1) → provision (M2b-2a) → mount (M2b-2b)**. It consumes the `VolumeStatus`
resources the provisioner publishes and mounts each known volume idempotently, publishing
`MountStatus`. It is non-destructive (mounting only; unmounting is deferred).

## 2. Goals / Non-goals

### Goals
- Extend the `Platform` trait with `is_mounted(target)` (the idempotency check), implemented by
  `LinuxPlatform` (reads `/proc/self/mountinfo`) and `FakePlatform` (checks recorded mounts).
- Add a `MountStatus` resource.
- Add a `VolumeMountController` that mounts each provisioned system volume at its fixed mountpoint
  (EFI→`/boot`, STATE→`/system/state`, EPHEMERAL→`/var`) iff not already mounted, and publishes
  `MountStatus`.
- Wire the controller into `machined` after the provisioner.

### Non-goals (deferred)
- **Unmounting** and the mount teardown lifecycle (M5; the controller is mount-only).
- Config-declared / user-disk extra mounts (fixed system layout only).
- Bind mounts, overlay, sub-mounts, mount-option tuning per volume, remount-on-change.
- Bootloader content on the EFI partition (the partition is mounted only).
- Encryption/LUKS, swap activation.

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `platform` | + `Platform::is_mounted(&self, target: &str) -> Result<bool>`. `LinuxPlatform` parses `/proc/self/mountinfo`; `FakePlatform` returns whether a recorded mount targets it. `mount` and its `create_dir_all(target)` already exist (M1) and work for real filesystems. |
| `resources` | + `MountStatus { volume, source, target, fstype, mounted }` + new `ResourceType` variant. |
| `controllers` | + `block::mount` module: a fixed `mountpoint(label) -> Option<&'static str>` map + `VolumeMountController`. |
| `machined` | register `VolumeMountController` after `VolumeProvisionerController`. |

### 3.2 `Platform::is_mounted`

```text
is_mounted(&self, target: &str) -> Result<bool>
```

- **`LinuxPlatform`**: read `/proc/self/mountinfo` and return whether any entry's mount point (field
  5) equals `target`. (Path injectable for tests is unnecessary — reading `/proc` is unprivileged and
  always reflects the live mount table.)
- **`FakePlatform`**: return whether any recorded `MountSpec` has `target == target` (the fake already
  records every `mount` call), so a controller test sees the second reconcile skip an already-mounted
  target.

### 3.3 `MountStatus` resource

```text
MountStatus { volume: String, source: String, target: String, fstype: String, mounted: bool }
```

Namespace `block`. Id = volume label (e.g. `STATE`). Observed state of a managed mount.

### 3.4 The mountpoint map

```text
mountpoint(label: &str) -> Option<&'static str>
    "EFI"       => "/boot"
    "STATE"     => "/system/state"
    "EPHEMERAL" => "/var"
    _           => None        // unknown volumes are not mounted
```

A pure function in the controller module; the only volumes mounted are the three system volumes.

### 3.5 `VolumeMountController`

- Inputs: `VolumeStatus` (Weak — published by M2b-2a's provisioner).
- Outputs: `MountStatus` (Exclusive, owned via `reconcile_owned`).
- Reconcile:
  1. List `VolumeStatus` in namespace `block`, keeping those with `phase == Provisioned`.
  2. For each, `mountpoint(label)`; skip if `None`.
  3. If `!platform.is_mounted(target)` → `platform.mount(MountSpec{ source: device, target,
     fstype: fs, flags: 0, data: None })` (the platform creates the target dir).
  4. Publish `MountStatus{ volume: label, source: device, target, fstype: fs, mounted: true }` for the
     volume, via `reconcile_owned` (so a volume that disappears from `VolumeStatus` has its
     `MountStatus` GC'd).
  - **Idempotent**: an already-mounted target is not re-mounted; it is still reported `mounted: true`.
  - On a mount/`is_mounted` error, the reconcile returns the error (the level-triggered loop retries);
    no `MountStatus` is published for the failed volume rather than a stale `mounted: false`. So a
    `MountStatus` exists only for volumes that are actually mounted. (`MountStatus.mounted` is `true`
    on every published status; the `bool` field is retained for forward use.)

**No discovery-style barrier is needed.** An empty `VolumeStatus` set means "nothing to mount" — a
harmless no-op (mounting is not destructive). The controller mounts each volume as its `VolumeStatus`
appears (re-woken by the Weak input), so it naturally trails the provisioner without a gate.

### 3.6 Wiring

`machined::run_daemon` registers `VolumeMountController` after `VolumeProvisionerController`. Order is
not a correctness requirement (the controllers are level-triggered reconcile loops; the mount
controller simply has nothing to do until `VolumeStatus` exists).

## 4. Error handling & observability

- A mount failure maps (via `ctl`) to a `runtime_core::Error` and the reconcile returns it; the
  level-triggered loop retries. No `MountStatus` is published for the failed volume (a present
  `MountStatus` therefore always means actually-mounted).
- `MountStatus` makes the mount state observable through the store (and the future API).

## 5. Testing strategy

- **Unit (root-free):**
  - `FakePlatform::is_mounted` — records a mount, asserts `is_mounted(target)` true and a different
    target false.
  - `LinuxPlatform::is_mounted` — reading `/proc/self/mountinfo` is unprivileged: assert `/` is
    mounted and a bogus path (`/no/such/mnt`) is not.
  - `mountpoint` map — the three labels map correctly; unknown → `None`.
  - `VolumeMountController` against `FakePlatform`: three provisioned `VolumeStatus` → three `mount`
    calls + three `MountStatus(mounted: true)`; a second reconcile → all already mounted → **no**
    additional `mount` calls (idempotent); a non-system label (e.g. `DATA`) is ignored.
- **Integration (privileged, gated):** a loopback test — `losetup` a file, `mkfs.ext4` a partition,
  then drive `LinuxPlatform::mount` + `is_mounted` to mount it at a temp target and assert it is
  mounted, then clean up. Gated behind root (`#[ignore]`).
- **CI:** `make pre-commit` for the unit tier; the privileged test runs separately.

## 6. Key risks

- **`/proc/self/mountinfo` parsing** — field 5 is the mount point, but mount points can contain
  octal-escaped characters (e.g. spaces as `\040`). For the fixed targets (`/boot`, `/system/state`,
  `/var`) this never bites, but the parser should split on whitespace and compare field 5 literally;
  a follow-up can add unescaping if user mounts arrive. Covered by the `/` and bogus-path unit test.
- **Mount target creation** — `LinuxPlatform::mount` already `create_dir_all`s the target
  best-effort; confirm it covers `/system/state` (a nested path that may not exist). The unit/loopback
  tests exercise this.
- **Idempotency correctness** — the `is_mounted` check must run before `mount`; the controller test
  asserts the second reconcile issues zero new `mount` calls.
