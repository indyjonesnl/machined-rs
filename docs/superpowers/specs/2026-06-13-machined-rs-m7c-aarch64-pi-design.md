# machined-rs M7c — aarch64 / Raspberry Pi, Design

**Date:** 2026-06-13
**Status:** Approved (brainstorming) — proceeds to implementation plan(s)
**Parent design:** `2026-06-12-machined-rs-m7-image-pipeline-design.md` (M7 build order: M7c = aarch64/Pi)
**Builds on:** M7a (bootable x86_64 image) + M7b (full-stack runtime), both merged to `main`, CI green.

## 1. Overview

M7c takes the image pipeline to aarch64 and the Raspberry Pi. The imager becomes arch-driven
(an `ArchConfig` table replaces the hardcoded x86 seams), gains an `aarch64` (generic qemu-virt)
configuration that CI can boot and assert end-to-end, and an `aarch64-rpi` (Pi 3A+) configuration
that CI builds and the operator verifies on real hardware.

Brainstorm decisions (user): target board **Pi 3A+** (512 MB, bcm2837, classic VideoCore firmware) ·
**qemu-system-aarch64 -M virt** automated CI boot gate · **Alpine aarch64 apks** for kernel/firmware ·
**split** into M7c-1 (generic aarch64 + CI) then M7c-2 (Pi firmware, manual verify).

## 2. Build order (one spec, two plans)

- **M7c-1 — generic aarch64 + automated boot.** Arch generalization; Alpine aarch64 linux-virt +
  arm64 containerd/runc; cross-compile; a `qemu-system-aarch64 -M virt` CI boot test asserting the
  full bar (API up + volumes provisioned + `RuntimeReady=true`). Fully automatable proof that the
  aarch64 binary and arm64 runtime run.
- **M7c-2 — Pi 3A+ firmware.** Alpine aarch64 linux-rpi + raspberrypi-bootloader; Pi/SD module set;
  Pi-firmware staging (blobs + DTB + generated config.txt/cmdline.txt). CI builds it; operator
  verifies on the Pi 3A+ via a documented checklist.

## 3. M7c-1 — generic aarch64 + CI boot

### 3.1 Arch generalization (imager)

Replace the hardcoded x86 assumptions in `crates/imager/src/build.rs` and `modules.rs` with a small
table:

```text
ArchConfig {
    kernel_path:  &str,        // e.g. "boot/vmlinuz-virt"
    module_roots: &[&str],     // virtio set, etc.
    console:      &str,        // "ttyS0" (x86) | "ttyAMA0" (aarch64 virt)
    firmware:     Option<PiFirmware>,  // None for virt; Some(...) for rpi (M7c-2)
}
```

- `arch_config(arch) -> ArchConfig` for `"x86_64"`, `"aarch64"` (M7c-1), `"aarch64-rpi"` (M7c-2).
- `build.rs` reads `kernel_path` + `module_roots` from it instead of the literal `boot/vmlinuz-virt`
  / `X86_64_QEMU_MODULES`.
- `--arch` (`main.rs`) accepts the three values.
- **Plan-time verification:** download the Alpine aarch64 `linux-virt` apk and confirm the kernel
  path. The qemu-virt machine uses virtio on both arches, so the **module set is expected to be
  identical to x86** (`virtio_blk, virtio_net, ext4, vfat, nls_cp437, nls_iso8859_1, nls_utf8`) — if
  so, rename the const to a shared `VIRT_MODULES` rather than duplicating it. Confirm the aarch64
  kernel path matches `boot/vmlinuz-virt`; if it differs, the table captures it.

### 3.2 Artifacts (aarch64 section)

New `[artifact] aarch64 = [...]` in `artifacts.toml`, sha256-pinned (download + verify at plan time,
never trust a supplied hash):

- Alpine v3.21 **aarch64** apks: `linux-virt`, `musl`, `e2fsprogs`(+`-libs`), `libcom_err`,
  `libblkid`, `libeconf`, `libuuid` — the aarch64 URLs of the same packages M7a/M7b pinned for x86.
- **arm64** static runtime: `containerd-static-<ver>-linux-arm64.tar.gz` (boot-tarball) +
  `runc.arm64` (boot-binary, rename `runc`).

The manifest is already keyed by arch (`BTreeMap<String, Vec<Artifact>>`); the boot-tarball /
boot-binary kinds and the apk extractor are arch-agnostic.

### 3.3 Cross-compilation

- `make dist-aarch64`: `cargo build --release --target aarch64-unknown-linux-musl -p machined`,
  mirroring `dist-x86_64`'s fallback chain (musl cross-linker if present, else a `CC_…=…-gcc`
  override). Tooling settled at plan time — likely `gcc-aarch64-linux-gnu` as the cross C compiler
  for `ring`'s C bits, with the musl target; `tonic`/`prost` use vendored protoc (host tool, no
  cross concern).
- CI tool image (`ci/Dockerfile`): add `--target aarch64-unknown-linux-musl` to the rustup install
  and the aarch64 cross toolchain package(s). Published the same way (ci-image.yml).

### 3.4 Boot test + CI

- Generalize `scripts/boot-test-x86_64.sh` into an arch-parameterized script (or add
  `boot-test-aarch64.sh` sharing the assertion logic): for aarch64 it runs
  `qemu-system-aarch64 -M virt -cpu <cortex-a53|max> -m 512` with `-kernel/-initrd`, `console=ttyAMA0`,
  virtio disk+net + hostfwd, and asserts the **same** bar as x86: API answers over mTLS, STATE+
  EPHEMERAL provisioned, `RuntimeStatus.ready=true`. No KVM for cross-arch → TCG; use a generous
  timeout.
- CI job `boot-test-aarch64` (in the container image, no `--device /dev/kvm` needed since TCG): build
  the aarch64 image, boot it, assert. **Fallback** (documented): if the arm64 runtime proves flaky
  under TCG or RuntimeReady is unreliable, drop that job's bar to API-up + volumes-provisioned and
  leave RuntimeReady to the Pi/manual path — but target full parity first.

## 4. M7c-2 — Pi 3A+ firmware

### 4.1 Artifacts (aarch64-rpi)

A distinct config — the Pi needs a different kernel + firmware than qemu-virt. Either a new manifest
arch key `aarch64-rpi`, or an `aarch64` variant; the plan picks the cleaner shape. Artifacts:

- Alpine aarch64 `linux-rpi` (the Raspberry Pi kernel flavor — ships the Pi kernel + `dtbs-rpi/`).
- Alpine aarch64 `raspberrypi-bootloader` (the VideoCore GPU firmware: `bootcode.bin`, `start.elf`,
  `fixup.dat`, and friends — installs under `/boot`).
- Same musl/e2fsprogs aarch64 apks + arm64 containerd/runc as M7c-1.

### 4.2 Pi module set + ArchConfig

`aarch64-rpi` `ArchConfig`: the Pi kernel path; the **SD/MMC module roots** (`mmc_block`, `sdhci`,
`sdhci_iproc` / the bcm2837 SD driver, plus `ext4`, `vfat`, `nls_*`) — the Pi boots off the SD card,
not virtio. (Many drivers are built into the Pi kernel; the closure resolver only includes what's a
module — verify which are `=m` vs builtin in the linux-rpi config at plan time.)

### 4.3 Pi-firmware staging

For `aarch64-rpi`, after the apks extract, a staging step copies onto the FAT (in addition to the
initramfs.img/config.yaml/pki/bin already staged):

- The GPU firmware blobs (`bootcode.bin`, `start.elf`, `fixup.dat`, …) from the extracted
  `raspberrypi-bootloader`.
- The Pi kernel image (named as `config.txt` expects) and `bcm2837-rpi-3-a-plus.dtb` (Pi 3A+).
- **Generated** `config.txt`:
  ```
  arm_64bit=1
  kernel=<kernel-image-name>
  initramfs initramfs.img followkernel
  enable_uart=1
  # device_tree=bcm2837-rpi-3-a-plus.dtb  (or rely on firmware auto-DTB)
  ```
- **Generated** `cmdline.txt`: `console=ttyAMA0,115200` (+ any needed args; machined is `/init` in
  the initramfs, so no `root=`).

The 512 MB Pi 3A+ memory split (`gpu_mem`) and the initramfs `followkernel` placement are verified on
hardware (documented).

### 4.4 Verification

- CI **builds** the `aarch64-rpi` image (proves cross-compile + Pi staging produce a complete FAT).
  **No automated boot** (qemu does not faithfully emulate the Pi 3A+ VideoCore firmware boot).
- **Manual checklist** (operator, documented in the plan/README): `dd`/flash the image to an SD card,
  boot the Pi 3A+ with a serial cable (115200) and/or network, watch the serial log for machined
  coming up, then `machinectl --endpoint https://<pi-ip>:50000 get …` over the network to confirm
  the API, provisioning, and (if containerd ran) RuntimeReady.

## 5. Risks

- **Pi 3A+ GPT vs MBR boot (top risk).** Modern Alpine/Pi firmware reads GPT, but the Pi 3 lineage
  historically wanted an MBR with the FAT as the first partition. If the board won't boot the GPT
  image, the image writer needs an MBR mode for `aarch64-rpi`. Discovered on hardware; the plan
  documents an MBR-fallback path for the writer.
- **qemu-system-aarch64 TCG speed/flakiness.** Cross-arch emulation is slow; the CI job gets a
  generous timeout + serial-log artifact. If RuntimeReady is unreliable under TCG, drop that job's
  bar (4.x fallback).
- **linux-rpi module layout / builtin-vs-module.** The Pi kernel may build the SD/MMC drivers in;
  the module closure must only list `=m` modules — verify against the linux-rpi config at plan time.
- **arm64 containerd/runc asset names/availability** — verify the exact release asset names + shas
  at plan time (download, never trust a supplied hash).
- **Pi firmware file set** — the exact blob list for a 64-bit Pi 3A+ boot (bootcode.bin + start.elf +
  fixup.dat vs the start4 set) confirmed against the raspberrypi-bootloader apk contents at plan time.

## 6. Non-goals

- A faithful Pi-firmware boot in CI (qemu can't; manual hardware only).
- Pi 4 / Pi 5 specifics (different firmware/DTBs) — Pi 3A+ is the named target; the ArchConfig table
  makes adding boards later cheap.
- u-boot / netboot / USB-boot paths; A/B upgrade; secure boot.
- CNI / pod launch on arm64 (the pod-launch follow-ups apply to all arches; out of scope here).

## 7. Testing

- M7c-1: imager unit tests for the `ArchConfig` selection + the aarch64 manifest parse (real
  `artifacts.toml` load); the **qemu-system-aarch64 CI boot test** (the integration gate — full
  parity bar). Cross-compile proven by `make dist-aarch64` building a static aarch64 binary.
- M7c-2: imager unit tests for Pi-firmware staging (config.txt/cmdline.txt generated correctly; the
  firmware blobs + DTB land on the FAT — read back from the built image); CI **build** of the
  aarch64-rpi image (no boot). Manual: the Pi 3A+ hardware checklist.
