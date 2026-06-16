# Pi-native A/B upgrade backend (`PiBootBackend`) (design)

**Date:** 2026-06-16
**Status:** Approved (design).
**Builds on:** M9b-1 (disk-persistent A/B upgrade on x86): the `BootloaderBackend` trait, `Slot`,
`parse_slot`, `prepare_disk → stage_inactive → set_active → FinalAction::Reboot`, and the
imager's `/A`//`/B` slot model.

## Goal

Give the **Raspberry Pi 3A+** the same disk-persistent A/B OS upgrade x86 got in M9b-1, using the
Pi's native firmware boot path (no UEFI/systemd-boot). An upgrade stages the new kernel+initramfs
into the inactive slot, flips the boot pointer, and cold-reboots; the VideoCore firmware boots the
new slot. The end-to-end upgrade is verified on real Pi 3A+ hardware (qemu cannot run the firmware
that reads `config.txt`/`os_prefix`).

This makes the upgrade story cover the project's actual small-ARM target, not just x86.

## The Pi boot reality (drives the design)

The Pi 3 boots: VideoCore ROM → `bootcode.bin` → `start.elf` → reads **`config.txt`** from the
(single, MBR) FAT partition → loads the kernel + DTB + initramfs named there. No UEFI, so
systemd-boot can't run. The chosen A/B primitive is **`os_prefix`** (a core `config.txt` directive,
reliably supported on Pi 3): any value in `os_prefix` is prepended to *every* OS file the firmware
loads — kernel, initramfs, `cmdline.txt`, the `.dtb`, and `dtoverlay` `.dtbo` files. With a trailing
`/` it is a subdirectory. So `os_prefix=A/` boots a self-contained slot dir `/A`.

`autoboot.txt`/`tryboot` (partition-level A/B with native one-shot auto-rollback) was rejected: it is
a Pi-4 EEPROM-bootloader feature with uncertain/unreliable Pi 3 support and needs two FAT partitions.
`os_prefix` mirrors x86's `/A`//`/B` layout exactly and is firmware-version-robust on the Pi 3.

## Scope

This is the **M9b-1 analog for the Pi**: A/B persist + explicit commit + cold-reboot into the new
slot. Out of scope: tryboot-style auto-rollback (the Pi-4 path; the M9b-2 analog), and Pi persistent
STATE/EPHEMERAL volumes (a separate roadmap item — the Pi image stays ephemeral, PKI re-seeds from
the shared `/boot` each boot). x86 is unaffected.

---

## Components

### 1. Pi A/B layout (`os_prefix`, mirrors x86)

One MBR FAT partition. **Read before `os_prefix` applies, so they stay at the FAT root:** the
firmware blobs (`bootcode.bin`, `start.elf`, `fixup.dat`) and `config.txt` itself. The shared payload
(`pki/`, `bin/`, `cni/`, `config.yaml`) also stays at the root (unchanged, `/boot`-relative).

```
/  bootcode.bin start.elf fixup.dat        VideoCore firmware (shared, root)
   config.txt                              has "os_prefix=A/"  ← THE pointer machined rewrites
   pki/ bin/ cni/ config.yaml              shared payload (root)
/A vmlinuz initramfs.img <dtb> cmdline.txt overlays/   slot A (cmdline.txt: …machined.slot=a)
/B vmlinuz initramfs.img <dtb> cmdline.txt overlays/   slot B (cmdline.txt: …machined.slot=b)
```

A **slot** is everything the firmware loads *after* `config.txt`: `vmlinuz`, `initramfs.img`, the DTB
(`bcm2837-rpi-3-a-plus.dtb`), `cmdline.txt`, and `overlays/` (the `.dtbo` files `config.txt`'s
`dtoverlay=` lines reference — **notably `disable-bt`, which the GPIO serial console depends on**).
The imager **scaffolds both slots** at build time (DTB + per-slot `cmdline.txt` + overlays); `/A` also
gets the real `vmlinuz`+`initramfs.img`, `/B` gets the scaffolding only (the first upgrade stages its
kernel). This mirrors x86, where the imager pre-writes both `a.conf` and `b.conf`.

`config.txt` keeps `kernel=vmlinuz`, `initramfs initramfs.img followkernel`,
`device_tree=bcm2837-rpi-3-a-plus.dtb`, `dtoverlay=disable-bt`, etc. — those names now resolve
*inside* the active slot via `os_prefix`.

### 2. `PiBootBackend` (`crates/machined`, implements the existing `BootloaderBackend` trait)

```rust
fn current_slot(&self) -> Slot   // parse_slot(/proc/cmdline) — REUSED from M9b-1; the firmware
                                 //   passes the slot's cmdline.txt (…machined.slot=a|b) as the
                                 //   kernel cmdline. Identical mechanism to x86.
fn stage_inactive(&self, kernel, initrd) -> Result<Slot>  // remount /boot rw; write
                                 //   /<other>/{vmlinuz,initramfs.img}; fsync; remount ro.
                                 //   (DTB/cmdline.txt/overlays already present from scaffolding.)
fn set_active(&self, slot) -> Result<()>   // rewrite config.txt's `os_prefix=<slot>/` line,
                                 //   PRESERVING every other line; fsync file + dir; (within an
                                 //   rw window, same remount discipline as SdBootBackend).
```

`set_active` is a line-oriented rewrite: replace the single `os_prefix=…` line's value (the imager
always writes one), leaving all other `config.txt` directives intact. The remount-rw/ro window,
best-effort ro re-seal + `warn!`, and fsync-the-pointer durability all mirror `SdBootBackend` (and
can share a small helper where clean). The upgrade abort property is inherited unchanged: a failed
stage/flip publishes `UpgradeStatus=Failed` and leaves the node booting the current slot.

`Slot::dir()` ("A"/"B") and `Slot::id()` ("a"/"b") are reused; the `os_prefix` value is
`format!("{}/", slot.dir())` → `A/`/`B/`.

### 3. machined backend selection (`crates/machined/src/main.rs`)

The same aarch64 `machined` binary runs on the Pi and on aarch64-virt, so the backend is chosen at
runtime from a tiny imager-baked **initramfs marker** `/etc/machined/bootloader` (parallel to
`/etc/machined/image-id`): `"pi"` → `PiBootBackend`, anything else (incl. absent) → `SdBootBackend`.

```rust
let upgrade_backend: Arc<dyn bootloader::BootloaderBackend> = match read_bootloader_marker().as_str() {
    "pi" => Arc::new(bootloader::PiBootBackend::new("/boot", platform.clone(), &cmdline)),
    _    => Arc::new(bootloader::SdBootBackend::new("/boot", platform.clone(), &cmdline)),
};
```

Everything downstream — the `Upgrade` action, `prepare_disk`, `FinalAction::Reboot`, the booted-slot
log — is **unchanged**. This milestone is a new backend + a constructor swap.

### 4. imager changes (`crates/imager`, `aarch64-rpi` arch)

- **`rpi.rs`**: `config_txt()` gains an `os_prefix=A/` line. A new step assembles the A/B slot layout:
  move `vmlinuz`+`initramfs.img` into `/A`; stage the DTB + `overlays/` (incl. `disable-bt.dtbo`,
  sourced from the `linux-rpi` apk's `boot/overlays/`) into **both** `/A` and `/B`; write per-slot
  `cmdline.txt` = the existing `console=…` line **plus `machined.slot=a|b`** (replacing the single
  root `cmdline.txt`). Firmware blobs + `config.txt` stay at root.
- **`build.rs` / `initramfs.rs`**: write the `/etc/machined/bootloader` marker into the initramfs —
  `"pi"` for `aarch64-rpi`, `"sdboot"` for the GPT arches (x86_64/aarch64). (Mirrors how
  `image-id` is baked.)
- The MBR image writer (`image.rs`) is unchanged (still one FAT partition); only the staging tree
  it copies changes.

### 5. Testing

- **CI (automated):**
  - `PiBootBackend` unit tests: against a temp `/boot` dir — `stage_inactive` writes
    `/B/{vmlinuz,initramfs.img}`; `set_active` rewrites only the `os_prefix` line of a multi-line
    `config.txt` (other lines preserved) and is fsync-durable; `current_slot` via `parse_slot`.
  - `rpi.rs` unit tests + the FAT-readback layout test (extend `build-test-aarch64-rpi.sh` /
    `rpi.rs` tests): assert `config.txt` has `os_prefix=A/`, `/A` has the kernel+initramfs+dtb+
    `cmdline.txt`(`machined.slot=a`)+`overlays/disable-bt.dtbo`, `/B` is scaffolded (dtb+cmdline.txt
    `machined.slot=b`+overlays, no kernel yet), and the `bootloader=pi` marker is in the initramfs.
  - A `bootloader`-selection unit test: marker `"pi"` selects `PiBootBackend`.
- **Hardware (operator, documented in `docs/raspberry-pi-3a-plus.md`):** flash; boot `/A` as v1 over
  serial; `machinectl upgrade <v2-bundle> <sha>`; observe machined stage `/B` + flip `os_prefix=B/`
  + cold-reboot; confirm it returns as v2 (`machinectl version` → `image_id=v2`) booted from `/B`;
  document the manual rollback (re-flip `os_prefix=A/` → boots v1).

### 6. Out of scope

tryboot/`autoboot.txt` auto-rollback (Pi-4 path; the M9b-2 analog); Pi persistent STATE/EPHEMERAL;
per-slot shared payload (containerd/cni binaries stay shared at root, as on x86); changing the x86
path; SecureBoot.

---

## Risks / watch-outs

- **`os_prefix` prepends to overlays → `disable-bt` MUST be in each slot, or the serial console dies.**
  The GPIO console relies on `dtoverlay=disable-bt` (PL011 on the header). Since `os_prefix` prefixes
  the overlay path too, `overlays/disable-bt.dtbo` must exist in `/A` and `/B`. Stage it (from the
  `linux-rpi` apk) into both slots. This is the single most likely first-boot failure and the first
  thing the hardware test checks.
- **`config.txt` rewrite must preserve all other lines.** `set_active` replaces only the
  `os_prefix=` line. A naive full-file rewrite that drops the firmware directives would brick the
  boot. Unit-test the preserve-other-lines behavior on a multi-line `config.txt`.
- **Firmware blobs are NOT `os_prefix`-prefixed** (loaded before `config.txt`), so an upgrade can't
  change them — acceptable (firmware rarely changes), mirrors x86's shared sd-boot binary.
- **DTB duplicated per slot** (~21 KB ×2) — negligible; keeps slots self-contained.
- **Backend selection marker absent → defaults to `sdboot`.** The imager always writes it for
  `aarch64-rpi`; a hand-built image without it would mis-select. The marker is the single source of
  truth; document it.
- **No qemu end-to-end.** The firmware `os_prefix` boot-selection is hardware-only; CI proves the
  layout + the backend's disk operations, the Pi proves the firmware actually boots the flipped slot.
  Same posture as the existing Pi hardware-verify (M7c-2) — necessary but the automated tests carry
  the regression weight.
- **`/B` kernel absent until first upgrade.** Booting `os_prefix=B/` before an upgrade would fail
  (no `/B/vmlinuz`); the imager ships `os_prefix=A/`, and `set_active(B)` is only called after
  `stage_inactive` populates `/B`. The prepare-then-commit ordering (stage before flip) guarantees
  this.
