# M9b-1 — Disk-persistent A/B upgrade (boot from disk + cold-reboot-to-v2) (design)

**Date:** 2026-06-15
**Status:** Approved (design); M9b decomposed into M9b-1 (this) + M9b-2 (health-gated auto-rollback).
**Builds on:** M9a (in-memory kexec upgrade) — reuses `prepare`'s download → verify → extract.

## Goal

machined persists an OS upgrade to disk in an **A/B slot** layout and boots the new image from
disk, so the upgrade **survives a cold reboot** (a real power cycle), not just an in-memory kexec.
The x86_64 boot test proves it end-to-end: the node boots **from its own disk** (UEFI firmware →
systemd-boot → active slot), an upgrade writes v2 to the inactive slot and flips the boot pointer,
and after a **cold reboot** the node comes back reporting the new image identity with STATE + PKI
intact.

This also delivers the long-pending **x86 bare-metal self-boot** (a real on-disk bootloader instead
of qemu's external `-kernel`).

## Why this is split out of M9b

M9b = disk A/B persistence + health-gated auto-rollback. Those are separable:

| | Scope | Proves |
|---|---|---|
| **M9b-1** (this) | boot from disk + A/B slots + persist upgrade + **explicit** commit + cold-reboot-to-v2 | the new image survives a power cycle; STATE/PKI persist across it |
| **M9b-2** | boot-once → health-confirm → **auto-rollback** (sd-boot boot-counting + machined confirm/revert) | a broken upgrade auto-recovers to the previous slot, even on kernel panic |

M9b-1 de-risks the brand-new boot-from-disk path before layering rollback semantics on top.

## The testability problem this solves (drives the design)

CI boots x86/aarch64 via qemu's **external `-kernel`/`-initrd`** — nothing boots *from the disk*, so a
cold reboot always reloads qemu's v1 kernel regardless of what is on disk. M9a's kexec upgrade is
in-memory for exactly this reason. To *prove* cold-reboot-to-v2 under qemu, the node must boot from
the disk: M9b-1 puts a real bootloader (systemd-boot) on the disk and switches the x86 test to UEFI
(OVMF) disk boot.

## The reference: Talos Linux

Talos (whose init daemon is literally `machined`) uses a dedicated BOOT partition with two slots
(`talos-A` / `talos-B`) + a small META key-value partition; **systemd-boot for UEFI**, GRUB only for
legacy BIOS. Upgrade writes the new version to the inactive slot, points the bootloader at it, and
reboots; the previous slot stays as fallback. M9b-1 adopts the systemd-boot half (the modern UEFI
path); M9b-2 will add Talos's boot-once → verify → commit-or-auto-rollback.

Talos's sd-boot A/B is its **x86/UEFI** path; on SBCs it uses native boot (u-boot), not sd-boot —
mirrored here by deferring the Pi to a native backend (below).

---

## Components

### 1. Disk layout & slot model

The existing single **EFI/ESP** FAT32 partition (type EFI System, label `EFI`) becomes the boot
partition. `STATE` + `EPHEMERAL` ext4 are still appended by machined on first boot — **unchanged**.
No separate BOOT/META partitions: the ESP + `loader.conf` carry everything (`loader.conf default` is
a plain file machined controls, robust without relying on EFI-variable persistence on a headless
node).

ESP contents:

```
/EFI/BOOT/BOOTX64.EFI       systemd-boot (firmware's default removable-media path; BOOTAA64.EFI on aarch64)
/loader/loader.conf         "default <slot>" + "timeout 0"   ← THE boot pointer machined flips
/loader/entries/a.conf      title / linux /A/vmlinuz / initrd /A/initramfs.img / options <cmdline> machined.slot=a
/loader/entries/b.conf      (same, slot b)
/A/{vmlinuz, initramfs.img} slot A
/B/{vmlinuz, initramfs.img} slot B (absent on a fresh image; created by the first upgrade)
config.yaml, pki/, bin/, cni/, images/    shared payload at the ESP root
```

A **slot = the `{vmlinuz, initramfs.img}` pair** + its loader entry. Exactly two slots, A and B.

**Scope decision (documented limitation):** a slot is *only* kernel + initramfs (consistent with
M9a's bundle). The shared `/boot` payload (containerd/runc/cni binaries, config, pki) stays shared,
so an upgrade cannot swap those binaries in M9b-1. The initramfs *is* the OS rootfs; kernel+initramfs
is the headline upgrade unit. Per-slot payload is out of scope (later milestone).

### 2. `BootloaderBackend` trait + slot identity (`crates/machined`, new module)

All bootloader specifics live behind a trait so the upgrade logic is platform-agnostic:

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot { A, B }

trait BootloaderBackend {
    /// Which slot the running kernel booted from (parsed from /proc/cmdline).
    fn current_slot(&self) -> Slot;
    /// Write kernel+initramfs into the inactive slot; returns that slot.
    fn stage_inactive(&self, kernel: &Path, initrd: &Path) -> anyhow::Result<Slot>;
    /// Flip the boot pointer to `slot` (the M9b-1 "commit").
    fn set_active(&self, slot: Slot) -> anyhow::Result<()>;
    // M9b-2 adds: arm boot-once on a slot, commit() after health check, etc.
}
```

- **`SdBootBackend`** (M9b-1; x86 + aarch64-UEFI): `stage_inactive` remounts the ESP rw, writes
  `/<slot>/{vmlinuz,initramfs.img}`, fsyncs, remounts ro; `set_active` rewrites `loader.conf`'s
  `default` line.
- **Slot identity:** each slot's loader entry bakes `machined.slot=a|b` into the kernel cmdline;
  `current_slot()` reads `/proc/cmdline`. Backend-agnostic — the Pi's `cmdline.txt` carries the same
  token. `inactive = the other slot`.
- A future **`PiBootBackend`** implements the same trait over `autoboot.txt` / `tryboot` (no UEFI);
  built + hardware-verified in a later step (qemu cannot boot the Pi from disk — see the raspi3ap
  finding in `scripts/boot-test-aarch64-rpi.sh`).

### 3. Upgrade-persist flow (`crates/machined`, extends M9a)

The `Upgrade` action's `prepare` is extended (or a sibling `prepare_disk` is added):

1. Download → verify sha256 → extract `vmlinuz` + `initramfs.img` (M9a, unchanged).
2. `slot = backend.stage_inactive(kernel, initrd)`.
3. `backend.set_active(slot)` — the explicit commit.
4. Final action = **cold `reboot`** (`FinalAction::Reboot`, not `Kexec`): the cold reboot through
   firmware + sd-boot is what proves disk persistence. The previous slot is untouched = the rollback
   target M9b-2 automates.

Any failure publishes `UpgradeStatus=Failed` and keeps the node up (the M9a graceful-abort property
is preserved). `UpgradePhase` gains a `Staged` variant (written-to-disk, pointer-flipped, about to
reboot) alongside the existing `Loaded` (kexec) — or `Loaded` is reused with an updated message; the
plan picks one and pins it in a test.

A new platform/block op remounts the ESP read-write for the staging window and back to read-only
(today `mount_boot` mounts `/boot` `MS_RDONLY`). The ESP device is the one `mount_boot` already
resolved.

### 4. Imager changes (`crates/imager`)

- Stage systemd-boot to `/EFI/BOOT/BOOTX64.EFI` (+ `BOOTAA64.EFI` for aarch64); write
  `loader/loader.conf` (`default a`, `timeout 0`) and `loader/entries/{a,b}.conf`; put the kernel +
  initramfs under `/A`; leave `/B` for the first upgrade.
- The systemd-boot EFI binary(ies) become **pinned `artifacts.toml` entries** (a new artifact
  `kind`, extracted from a stable `systemd-boot-efi` package — sourcing pinned by sha256 like every
  other artifact).
- Applies to the **GPT** arches (`x86_64`, `aarch64`); the Pi (`aarch64-rpi`, MBR) is **unchanged**
  (Pi backend later). `aarch64-mbr` (the test arch) is likewise unchanged.
- Slot A's entry cmdline carries `machined.slot=a` plus the usual `console=` etc.

### 5. x86 UEFI boot + CI (`scripts/boot-test-x86_64.sh`, `ci/Dockerfile`, `.github/workflows`)

- The x86 boot test switches from `qemu -kernel bzImage` to **OVMF + boot-from-disk**: load OVMF as
  pflash and pass only `-drive file=img` (no `-kernel`/`-initrd`). systemd-boot on the ESP boots the
  active slot.
- The CI tool image gains the **`ovmf`** package → a `ci/Dockerfile` change → the established
  **two-phase CI-image rollout** (Dockerfile change republishes `:latest`, then the boot job uses
  it).
- aarch64 already has UEFI firmware (`qemu-efi-aarch64`); switching the aarch64 boot test to disk
  boot is **optional** for M9b-1 and not required (its image gets the sd-boot layout regardless, but
  the asserted boot-from-disk path is x86).

### 6. Testing (`scripts/boot-test-x86_64.sh`)

1. Build the v1 image (sd-boot layout, slot A = v1, `--image-id v1`). Boot it **from disk** via OVMF;
   assert API up + `STATE`/`EPHEMERAL` Provisioned + `RuntimeReady` (as today).
2. Build a v2 bundle (`--image-id v2`), serve over http (`python3 -m http.server`, as M9a).
3. `ctl upgrade http://10.0.2.2:<port>/bundle.tgz <sha256>`. machined stages slot B + flips
   `loader.conf default=b` + **cold reboots**.
4. qemu re-reads the **same disk** (no `-no-reboot`) → OVMF → sd-boot → slot B. Assert:
   - `machinectl version` → `image-id=v2` (booted the new slot **from disk**, through firmware), AND
   - `STATE` + `EPHEMERAL` still `Provisioned`, `RuntimeStatus ready=true`, AND
   - the same machinectl client bundle still authenticates (STATE's CA persisted across a **real
     power cycle** — the proof M9a's in-memory kexec could not give).

### 7. Out of scope (→ M9b-2 / later)

- **Health-gated auto-rollback** (sd-boot boot-counting `+tries` / boot-once, machined health
  confirm, automatic revert to the previous slot) — M9b-2.
- **`PiBootBackend`** (`autoboot.txt`/`tryboot`, no UEFI) — designed against the trait here, built +
  hardware-verified later.
- **Per-slot payload** (containerd/runc/cni binaries, config, pki) — slots are kernel+initramfs only.
- **aarch64-virt switching to disk boot**; **SecureBoot / UKI**; **legacy-BIOS GRUB**; downgrade
  protection.

---

## Risks / watch-outs

- **systemd-boot binary sourcing.** It is a standalone PE EFI binary, arch-specific. Pin
  `BOOTX64.EFI`/`BOOTAA64.EFI` from a stable distro `systemd-boot-efi` package by sha256. Verify the
  pinned binary actually boots under OVMF in the boot test (the test is the arbiter).
- **ESP rw remount.** Writing a slot needs the ESP mounted rw; it is `ro` at runtime. Remount rw →
  write → fsync → remount ro, and treat any failure as `UpgradeStatus=Failed` (node stays up). A
  crash mid-stage leaves a partial inactive slot, but the **active** slot + pointer are untouched, so
  the node still boots — the partial slot is overwritten by the next upgrade.
- **OVMF in CI (two-phase).** Adding `ovmf` is a `ci/Dockerfile` change; the boot job must run on the
  republished `:latest`. Follow the two-phase rollout.
- **Cold reboot under qemu.** The boot test must NOT pass `-no-reboot`, so the guest's
  `reboot(RB_AUTOBOOT)` makes qemu reset and re-read the disk. The test waits for the API to drop,
  then answer again as v2.
- **Current-slot detection.** Relies on `machined.slot=` in the cmdline; if absent (older image /
  manual boot), default to `A` and log. The imager always writes it.
- **Image size on 512 MB.** sd-boot is ~100 KB; a second slot adds one kernel (~9 MB) + initramfs
  (~6–7 MB). The ESP is 512 MiB — ample. OVMF is qemu-host-only (not on the node).
- **Existing x86 kexec assertion.** M9a's in-memory kexec test path is superseded by the
  cold-reboot-to-v2 assertion; the plan decides whether to keep a kexec smoke assertion or replace it
  outright (kexec still works from a disk-booted node).
