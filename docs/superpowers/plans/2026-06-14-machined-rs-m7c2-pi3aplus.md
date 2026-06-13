# M7c-2 — Raspberry Pi 3A+ Firmware Image Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a flashable Raspberry Pi 3A+ SD-card image (`--arch aarch64-rpi`) that boots the aarch64 machined from the initramfs via the Pi GPU firmware, with CI building+validating the image (no boot) and the operator verifying on real hardware over serial.

**Architecture:** Introduce an `ArchConfig` table (now warranted — the Pi genuinely diverges) selecting kernel path, module set, partition scheme, and optional Pi-firmware. The Pi uses Alpine `linux-rpi` (raw arm64 `Image`, all SD/FS drivers built-in → empty initramfs module set), an **MBR** single-FAT partition (Pi 3 firmware reads MBR, not GPT), and FAT-staged GPU blobs + DTB + generated `config.txt`/`cmdline.txt`. Because MBR ≠ GPT, machined's `mount_boot` gains a vfat fallback so /boot mounts on the Pi. No STATE/EPHEMERAL auto-provisioning on Pi (that's GPT-only — documented).

**Tech Stack:** Rust, Alpine aarch64 `linux-rpi` + `raspberrypi-bootloader`(+`-common`), hand-rolled MBR writer, `fatfs`, the existing aarch64-musl machined binary (same as M7c-1 — board-independent).

**Decisions (user):** MBR single FAT (boots; no GPT provisioning) · serial-console-primary verification (ttyAMA0 via `dtoverlay=disable-bt`; machinectl-over-network optional via USB-ethernet).

**Plan-time verified facts (downloaded + computed 2026-06-14 — re-verify shas by download at pin time):**
- `linux-rpi-6.12.13-r0.apk` (aarch64, sha `3293972f523b9f833c091ad3de807294dde4c268a2756e5faeea8d6fbf6ff4ed`): kernel at **`boot/vmlinuz-rpi`** = a **raw uncompressed arm64 Image** (not gzip, not EFI-stub). kver `6.12.13-0-rpi`. Pi 3A+ DTB at **`boot/bcm2837-rpi-3-a-plus.dtb`**. **SD/MMC (sdhci-iproc, bcm2835-sdhost), ext4, fat, vfat, nls_cp437 are ALL BUILTIN** → the initramfs needs **ZERO storage/fs modules** (empty module set). (Modules in this apk are `.ko.xz`, but none are needed, so the `.ko.gz`-only extractor is irrelevant — they get pruned.)
- `raspberrypi-bootloader-1.20250210-r0.apk` (sha `1affbff4f11402aedcc5cdf4604d26e92422729378d23471a8e9d58ffd2a5451`): ships `boot/{start.elf, start4.elf, fixup.dat, fixup4.dat}`. `raspberrypi-bootloader-common-1.20250210-r0.apk` (sha `2813ec64112f96332c8ecbb8c65fe5623b423316ca0f2187a2132c60327be4e0`): ships `boot/bootcode.bin`. **Pi 3 boot needs: bootcode.bin + start.elf + fixup.dat** (start4/fixup4 are Pi4-only, harmless). Neither apk ships config.txt/cmdline.txt (no conflict).
- arm64 containerd `8f409c39562f11a116227e833797ab421a6ebde96f92aecd88ae0409a6bf1873`, runc.arm64 `633301e2e32f8a5ad54031aab4901eb00308bec677dd15faa2751e8f9dab5ca4` (same as M7c-1's aarch64 section).
- **Pi 3 reads MBR, not GPT** (bcm2837 boot ROM; multiple sources). Hence MBR.
- Code seams: `crates/imager/src/build.rs:88` `modules::VIRT_MODULES`, `:89` `rootfs.join("boot/vmlinuz-virt")`, FAT staging ~`:100-127`, `image::write_image(o.out, o.size, &staging)` ~`:128`; `find_kver` arch-agnostic. `crates/imager/src/main.rs:29` value_parser `["x86_64","aarch64"]`. `crates/imager/src/image.rs` `write_image` (GPT + protective MBR + EFI FAT32). `crates/imager/src/modules.rs` `VIRT_MODULES`. machined `crates/machined/src/imageboot.rs` `mount_boot` finds GPT `partition_label=="EFI"`; `crates/block` has `fsprobe` (fs-type detection) + sysfs partition enumeration.

---

### Task 1: ArchConfig table + --arch aarch64-rpi

**Files:**
- Create: `crates/imager/src/arch.rs`
- Modify: `crates/imager/src/main.rs` (mod + value_parser), `crates/imager/src/build.rs` (use ArchConfig), `crates/imager/src/modules.rs` (add `PI_MODULES`)

- [ ] **Step 1: Write the failing test** (`crates/imager/src/arch.rs`)

```rust
//! Per-architecture image parameters. x86_64 and aarch64 (qemu-virt) share the
//! same linux-virt kernel + virtio modules + GPT; aarch64-rpi (Raspberry Pi 3A+)
//! diverges: the linux-rpi kernel, an empty initramfs module set (SD/FS drivers
//! are builtin), an MBR partition table (Pi 3 reads MBR, not GPT), and Pi GPU
//! firmware staged on the FAT.

/// Partition table scheme for the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartScheme {
    /// GPT + protective MBR, single EFI-labeled FAT32 (x86/aarch64 virt).
    Gpt,
    /// One FAT32 primary partition in a classic MBR (Raspberry Pi 3).
    Mbr,
}

/// Per-arch build parameters.
#[derive(Clone, Debug)]
pub struct ArchConfig {
    /// Kernel image path inside the extracted rootfs.
    pub kernel_path: &'static str,
    /// Initramfs module roots (resolved against modules.dep). Empty = no modules.
    pub module_roots: &'static [&'static str],
    /// Partition table scheme.
    pub scheme: PartScheme,
    /// Some(()) when this arch needs Raspberry Pi firmware staging.
    pub rpi_firmware: bool,
}

/// Resolve the build parameters for an arch string. Returns None for unknown.
pub fn arch_config(arch: &str) -> Option<ArchConfig> {
    Some(match arch {
        "x86_64" | "aarch64" => ArchConfig {
            kernel_path: "boot/vmlinuz-virt",
            module_roots: crate::modules::VIRT_MODULES,
            scheme: PartScheme::Gpt,
            rpi_firmware: false,
        },
        "aarch64-rpi" => ArchConfig {
            kernel_path: "boot/vmlinuz-rpi",
            module_roots: crate::modules::PI_MODULES,
            scheme: PartScheme::Mbr,
            rpi_firmware: true,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virt_arches_share_gpt_and_virt_modules() {
        for a in ["x86_64", "aarch64"] {
            let c = arch_config(a).unwrap();
            assert_eq!(c.kernel_path, "boot/vmlinuz-virt");
            assert_eq!(c.scheme, PartScheme::Gpt);
            assert!(!c.rpi_firmware);
            assert!(!c.module_roots.is_empty());
        }
    }

    #[test]
    fn rpi_uses_rpi_kernel_mbr_empty_modules_firmware() {
        let c = arch_config("aarch64-rpi").unwrap();
        assert_eq!(c.kernel_path, "boot/vmlinuz-rpi");
        assert_eq!(c.scheme, PartScheme::Mbr);
        assert!(c.rpi_firmware);
        assert!(c.module_roots.is_empty(), "Pi SD/FS drivers are builtin");
    }

    #[test]
    fn unknown_arch_is_none() {
        assert!(arch_config("riscv").is_none());
    }
}
```

- [ ] **Step 2: Add `PI_MODULES`** to `crates/imager/src/modules.rs` (next to `VIRT_MODULES`):

```rust
/// The Raspberry Pi (linux-rpi) initramfs module roots. EMPTY: the Pi kernel
/// builds the SD/MMC host (sdhci-iproc, bcm2835-sdhost), ext4, fat/vfat, and
/// nls_cp437 in — the initramfs needs no storage/fs modules for an SD boot.
pub const PI_MODULES: &[&str] = &[];
```

- [ ] **Step 3: Wire it** — `crates/imager/src/main.rs`: add `mod arch;` and change `value_parser = ["x86_64", "aarch64"]` → `["x86_64", "aarch64", "aarch64-rpi"]`. In `crates/imager/src/build.rs`, replace the two hardcodes (lines ~88-89):

```rust
    let cfg = crate::arch::arch_config(o.arch)
        .ok_or_else(|| anyhow::anyhow!("unknown arch {}", o.arch))?;
    let mods = modules::resolve_closure(&dep, cfg.module_roots)?;
    let kernel = rootfs.join(cfg.kernel_path);
    anyhow::ensure!(kernel.exists(), "kernel {} missing from apk", cfg.kernel_path);
```

(`cfg` is reused in later tasks for `scheme` + `rpi_firmware`. Keep the existing `kernel_bytes = std::fs::read(&kernel)...` line after.)

- [ ] **Step 4: Run + gates**

Run: `cargo test -p machined-imager arch && cargo test -p machined-imager && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`
Expected: arch tests pass; existing x86/aarch64 build tests still pass (kernel path + VIRT_MODULES unchanged for them); the empty-closure case for PI_MODULES is covered by Task 4's build test.

Note: `resolve_closure(dep, &[])` must return an empty Vec (no roots → no modules). If it errors on empty roots, fix `resolve_closure` to return `Ok(vec![])` for empty input and add a unit test for it.

- [ ] **Step 5: Commit**

```bash
git add crates/imager/src/arch.rs crates/imager/src/main.rs crates/imager/src/build.rs crates/imager/src/modules.rs
git commit -m "feat(imager): ArchConfig table + --arch aarch64-rpi (Pi kernel, MBR, empty modules)"
```

---

### Task 2: Pin aarch64-rpi artifacts

**Files:**
- Modify: `crates/imager/artifacts.toml`, `crates/imager/src/manifest.rs` (extend real-manifest test)

- [ ] **Step 1: Re-verify shas by download**

```bash
cd /tmp && rm -rf m7c2 && mkdir m7c2 && cd m7c2
base=https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64
for f in linux-rpi-6.12.13-r0 raspberrypi-bootloader-1.20250210-r0 raspberrypi-bootloader-common-1.20250210-r0; do
  if curl -fsSL -o "$f.apk" "$base/$f.apk"; then echo "$(sha256sum "$f.apk")"; \
  else pkg=$(echo "$f" | sed -E 's/-[0-9].*//'); echo "MISSING $f:"; curl -sL "$base/" | grep -oE "${pkg}-[0-9][^\"]*\.apk" | sort -uV | tail -1; fi
done
# confirm kernel path + DTB + builtin SD drivers:
mkdir k && tar -xzf linux-rpi-6.12.13-r0.apk -C k 2>/dev/null
file k/boot/vmlinuz-rpi; ls k/boot/bcm2837-rpi-3-a-plus.dtb
grep -E 'sdhci|bcm2835.*sd|mmc_block|vfat|nls_cp437' k/lib/modules/*/modules.builtin | head
# firmware blobs:
mkdir fw && tar -xzf raspberrypi-bootloader-1.20250210-r0.apk -C fw 2>/dev/null && ls fw/boot/
mkdir fwc && tar -xzf raspberrypi-bootloader-common-1.20250210-r0.apk -C fwc 2>/dev/null && ls fwc/boot/bootcode.bin
```
(musl/e2fsprogs/libs + containerd/runc are identical to the M7c-1 `aarch64` section — reuse those shas/URLs.) If a linux-rpi/bootloader filename 404s (Alpine rolled it), use the dir-listing's current file and re-verify.

- [ ] **Step 2: Add the aarch64-rpi section** to `crates/imager/artifacts.toml` (after the `aarch64` array, same `[artifact]` table). Reuse the 6 aarch64 musl/e2fsprogs/libs apks + the arm64 containerd/runc verbatim from the `aarch64` section; swap `linux-virt` → `linux-rpi` and add the two firmware apks:

```toml

aarch64-rpi = [
  { name = "linux-rpi", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/linux-rpi-6.12.13-r0.apk", sha256 = "<COMPUTED>", kind = "apk" },
  { name = "raspberrypi-bootloader", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/raspberrypi-bootloader-1.20250210-r0.apk", sha256 = "<COMPUTED>", kind = "apk" },
  { name = "raspberrypi-bootloader-common", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/raspberrypi-bootloader-common-1.20250210-r0.apk", sha256 = "<COMPUTED>", kind = "apk" },
  # (copy the 6 aarch64 musl/e2fsprogs/e2fsprogs-libs/libcom_err/libblkid/libeconf/libuuid apk rows from the aarch64 section verbatim)
  { name = "containerd", url = "https://github.com/containerd/containerd/releases/download/v2.0.9/containerd-static-2.0.9-linux-arm64.tar.gz", sha256 = "8f409c39562f11a116227e833797ab421a6ebde96f92aecd88ae0409a6bf1873", kind = "boot-tarball" },
  { name = "runc", url = "https://github.com/opencontainers/runc/releases/download/v1.4.3/runc.arm64", sha256 = "633301e2e32f8a5ad54031aab4901eb00308bec677dd15faa2751e8f9dab5ca4", kind = "boot-binary", rename = "runc" },
]
```

The firmware apks are `kind = "apk"` — they extract into the rootfs (`boot/start.elf` etc.); Task 4 copies the needed blobs from the rootfs onto the FAT.

- [ ] **Step 3: Extend the real-manifest test** — in `crates/imager/src/manifest.rs` `real_artifacts_manifest_parses`, add:

```rust
    // aarch64-rpi section: Pi kernel + GPU firmware apks + arm64 runtime.
    let rpi = m.for_arch("aarch64-rpi").expect("aarch64-rpi arch present");
    assert!(rpi.iter().any(|a| a.name == "linux-rpi" && a.kind == "apk"));
    assert!(rpi.iter().any(|a| a.name == "raspberrypi-bootloader" && a.kind == "apk"));
    assert!(rpi.iter().any(|a| a.name == "raspberrypi-bootloader-common" && a.kind == "apk"));
    assert!(rpi.iter().any(|a| a.name == "runc" && a.kind == "boot-binary" && a.rename.as_deref() == Some("runc")));
```

- [ ] **Step 4: Run + gates**

Run: `cargo test -p machined-imager && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`
Expected: the real-manifest test parses all three arch sections.

- [ ] **Step 5: Commit**

```bash
git add crates/imager/artifacts.toml crates/imager/src/manifest.rs
git commit -m "feat(imager): pin aarch64-rpi artifacts (linux-rpi + Pi firmware apks)"
```

---

### Task 3: MBR partition-table writer

**Files:**
- Modify: `crates/imager/src/image.rs` (add `scheme: PartScheme` param + an MBR path)

- [ ] **Step 1: Failing test** — add to `crates/imager/src/image.rs` tests an MBR-mode build asserting the on-disk MBR + FAT:

```rust
#[test]
fn mbr_image_has_bootable_fat_primary_and_no_gpt() {
    use crate::arch::PartScheme;
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("pi.img");
    let staging = dir.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("config.txt"), b"arm_64bit=1\n").unwrap();

    write_image(&img, 1024 * 1024 * 1024, &staging, PartScheme::Mbr).unwrap();

    let raw = std::fs::read(&img).unwrap();
    // MBR boot signature.
    assert_eq!(&raw[510..512], &[0x55, 0xAA], "MBR signature");
    // Partition entry 1 @ 446: status 0x80 (bootable), type 0x0C (FAT32 LBA).
    assert_eq!(raw[446], 0x80, "partition 1 bootable");
    assert_eq!(raw[446 + 4], 0x0C, "partition 1 type FAT32 LBA");
    // LBA start (entry+8, u32 LE) = 2048 (1 MiB align); sector count > 0.
    let lba = u32::from_le_bytes(raw[446 + 8..446 + 12].try_into().unwrap());
    let cnt = u32::from_le_bytes(raw[446 + 12..446 + 16].try_into().unwrap());
    assert_eq!(lba, 2048, "FAT starts at LBA 2048");
    assert!(cnt > 0);
    // NOT a protective-GPT MBR: entry 1 type is 0x0C, not 0xEE; and no "EFI PART"
    // signature at LBA1 (offset 512).
    assert_ne!(raw[446 + 4], 0xEE, "not a protective GPT MBR");
    assert_ne!(&raw[512..520], b"EFI PART", "no GPT header");

    // The FAT region is a mountable FAT32 carrying the staged file.
    let file = std::fs::File::options().read(true).open(&img).unwrap();
    let slice = fscommon::StreamSlice::new(file, lba as u64 * 512, (lba as u64 + cnt as u64) * 512).unwrap();
    let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
    let names: Vec<String> = fs.root_dir().iter().map(|e| e.unwrap().file_name()).collect();
    assert!(names.contains(&"config.txt".to_string()), "{names:?}");
}
```

Also update the EXISTING gpt test(s) to pass `PartScheme::Gpt` to `write_image` (the signature gains a param).

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined-imager mbr_image`
Expected: FAIL — `write_image` takes 3 args / no `PartScheme`.

- [ ] **Step 3: Implement** — change `write_image` to take `scheme: crate::arch::PartScheme` and branch. Keep the existing GPT body for `Gpt`; add an MBR writer for `Mbr`:

```rust
pub fn write_image(img: &Path, size: u64, staging: &Path, scheme: crate::arch::PartScheme) -> anyhow::Result<()> {
    match scheme {
        crate::arch::PartScheme::Gpt => write_image_gpt(img, size, staging),
        crate::arch::PartScheme::Mbr => write_image_mbr(img, size, staging),
    }
}
```

Move the current body into `write_image_gpt`. Add `write_image_mbr`:

```rust
/// Classic MBR: one FAT32 primary partition (type 0x0C, bootable) starting at
/// LBA 2048, spanning the rest of the disk. Pi 3 firmware reads the MBR to find
/// the boot FAT (it does not parse GPT). No second partition — machined boots
/// entirely from the initramfs + this FAT.
fn write_image_mbr(img: &Path, size: u64, staging: &Path) -> anyhow::Result<()> {
    const LB: u64 = 512;
    const START_LBA: u64 = 2048; // 1 MiB alignment
    anyhow::ensure!(size > (START_LBA + 2048) * LB, "image too small");
    let mut file = std::fs::File::options().read(true).write(true).create(true).truncate(true).open(img)
        .with_context(|| format!("create {}", img.display()))?;
    file.set_len(size)?;

    let total_sectors = size / LB;
    let part_sectors = u32::try_from(total_sectors - START_LBA).unwrap_or(u32::MAX);
    let lba_start = START_LBA as u32;

    // Build the 512-byte MBR.
    let mut mbr = [0u8; 512];
    // Partition entry 1 at offset 446 (16 bytes).
    let e = 446;
    mbr[e] = 0x80;            // bootable
    // CHS start/end: use the LBA-only sentinel (0xFE 0xFF 0xFF) — modern + Pi firmware use LBA.
    mbr[e + 1] = 0xFE; mbr[e + 2] = 0xFF; mbr[e + 3] = 0xFF;
    mbr[e + 4] = 0x0C;       // type: FAT32 LBA
    mbr[e + 5] = 0xFE; mbr[e + 6] = 0xFF; mbr[e + 7] = 0xFF;
    mbr[e + 8..e + 12].copy_from_slice(&lba_start.to_le_bytes());   // first LBA
    mbr[e + 12..e + 16].copy_from_slice(&part_sectors.to_le_bytes()); // sector count
    mbr[510] = 0x55; mbr[511] = 0xAA;
    use std::io::{Seek, SeekFrom, Write};
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&mbr)?;

    // Format FAT32 in the partition region and populate from staging.
    let (start, end) = (START_LBA * LB, (START_LBA + part_sectors as u64) * LB);
    let file = std::fs::File::options().read(true).write(true).open(img)?;
    let mut slice = fscommon::StreamSlice::new(file, start, end)?;
    fatfs::format_volume(&mut slice, fatfs::FormatVolumeOptions::new()
        .fat_type(fatfs::FatType::Fat32).volume_label(*b"BOOT       "))?;
    let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new())?;
    copy_tree(&fs.root_dir(), staging)?;
    Ok(())
}
```

(`copy_tree` already exists from the GPT writer — reuse it. `Context` is imported. The CHS sentinel `0xFE 0xFF 0xFF` is the standard "use LBA" marker.)

- [ ] **Step 4: Update the caller** — in `crates/imager/src/build.rs`, change `image::write_image(o.out, o.size, &staging)` → `image::write_image(o.out, o.size, &staging, cfg.scheme)` (cfg from Task 1).

- [ ] **Step 5: Run + gates**

Run: `cargo test -p machined-imager && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`
Expected: the new MBR test + the updated GPT tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/imager/src/image.rs crates/imager/src/build.rs
git commit -m "feat(imager): MBR partition writer for aarch64-rpi (Pi 3 reads MBR)"
```

---

### Task 4: Pi-firmware staging (blobs + DTB + config.txt/cmdline.txt)

**Files:**
- Create: `crates/imager/src/rpi.rs`
- Modify: `crates/imager/src/main.rs` (`mod rpi;`), `crates/imager/src/build.rs` (call the staging step when `cfg.rpi_firmware`)

- [ ] **Step 1: Failing tests** (`crates/imager/src/rpi.rs`)

```rust
//! Raspberry Pi firmware staging onto the FAT boot partition: copy the GPU
//! blobs (bootcode.bin, start.elf, fixup.dat) + the Pi 3A+ DTB from the
//! extracted rootfs, and generate config.txt / cmdline.txt.

use anyhow::Context;
use std::path::Path;

/// The GPU firmware blobs a bcm2837 (Pi 3) boot needs, found under rootfs/boot/.
const PI3_BLOBS: &[&str] = &["bootcode.bin", "start.elf", "fixup.dat"];
const PI3_DTB: &str = "bcm2837-rpi-3-a-plus.dtb";

/// config.txt for a 64-bit Pi 3A+ booting kernel + initramfs (headless node).
pub fn config_txt() -> &'static str {
    "arm_64bit=1\n\
     kernel=vmlinuz\n\
     initramfs initramfs.img followkernel\n\
     gpu_mem=16\n\
     enable_uart=1\n\
     dtoverlay=disable-bt\n\
     device_tree=bcm2837-rpi-3-a-plus.dtb\n"
}

/// cmdline.txt — serial0 maps to whichever UART is on the GPIO header (PL011
/// with disable-bt). machined is /init in the initramfs, so no root=.
pub fn cmdline_txt() -> &'static str {
    "console=serial0,115200 console=tty1\n"
}

/// Stage Pi firmware: copy blobs + DTB from rootfs/boot into staging, write
/// config.txt + cmdline.txt. The kernel is already staged as `vmlinuz` by the
/// generic path (config.txt names it).
///
/// # Errors
/// Fails if a required blob/DTB is missing from the rootfs or on I/O error.
pub fn stage_pi_firmware(rootfs: &Path, staging: &Path) -> anyhow::Result<()> {
    let boot = rootfs.join("boot");
    for f in PI3_BLOBS {
        let src = boot.join(f);
        anyhow::ensure!(src.exists(), "Pi firmware blob {f} missing (raspberrypi-bootloader apks)");
        std::fs::copy(&src, staging.join(f)).with_context(|| format!("stage {f}"))?;
    }
    let dtb_src = boot.join(PI3_DTB);
    anyhow::ensure!(dtb_src.exists(), "Pi 3A+ DTB {PI3_DTB} missing (linux-rpi apk)");
    std::fs::copy(&dtb_src, staging.join(PI3_DTB)).with_context(|| format!("stage {PI3_DTB}"))?;
    std::fs::write(staging.join("config.txt"), config_txt()).context("write config.txt")?;
    std::fs::write(staging.join("cmdline.txt"), cmdline_txt()).context("write cmdline.txt")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_cmdline_have_the_pi3_essentials() {
        let c = config_txt();
        assert!(c.contains("arm_64bit=1"));
        assert!(c.contains("kernel=vmlinuz"));
        assert!(c.contains("initramfs initramfs.img followkernel"));
        assert!(c.contains("dtoverlay=disable-bt")); // PL011 on the header
        assert!(c.contains("device_tree=bcm2837-rpi-3-a-plus.dtb"));
        assert!(cmdline_txt().contains("console=serial0,115200"));
    }

    #[test]
    fn stages_blobs_dtb_and_generated_configs() {
        let dir = tempfile::tempdir().unwrap();
        let (rootfs, staging) = (dir.path().join("rootfs"), dir.path().join("staging"));
        std::fs::create_dir_all(rootfs.join("boot")).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        for f in PI3_BLOBS { std::fs::write(rootfs.join("boot").join(f), f.as_bytes()).unwrap(); }
        std::fs::write(rootfs.join("boot").join(PI3_DTB), b"dtb").unwrap();

        stage_pi_firmware(&rootfs, &staging).unwrap();

        for f in PI3_BLOBS { assert_eq!(std::fs::read(staging.join(f)).unwrap(), f.as_bytes()); }
        assert_eq!(std::fs::read(staging.join(PI3_DTB)).unwrap(), b"dtb");
        assert!(staging.join("config.txt").exists());
        assert!(staging.join("cmdline.txt").exists());
    }

    #[test]
    fn missing_blob_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (rootfs, staging) = (dir.path().join("rootfs"), dir.path().join("staging"));
        std::fs::create_dir_all(rootfs.join("boot")).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        // no blobs staged → error names the missing one
        let err = stage_pi_firmware(&rootfs, &staging).unwrap_err();
        assert!(err.to_string().contains("bootcode.bin"), "{err}");
    }
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined-imager rpi`
Expected: FAIL — module/functions missing.

- [ ] **Step 3: Wire into build.rs** — add `mod rpi;` to main.rs. In `build.rs`, after the existing FAT staging (vmlinuz/initramfs.img/config.yaml/pki) and before `image::write_image`, add:

```rust
    if cfg.rpi_firmware {
        crate::rpi::stage_pi_firmware(&rootfs, &staging)?;
    }
```

(The generic path already stages the kernel as `staging/vmlinuz` — config.txt's `kernel=vmlinuz` matches. The Pi blobs/DTB/config.txt/cmdline.txt ride alongside on the FAT.)

- [ ] **Step 4: Run + gates**

Run: `cargo test -p machined-imager && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`
Expected: rpi tests pass; the imager crate is green.

- [ ] **Step 5: Commit**

```bash
git add crates/imager/src/rpi.rs crates/imager/src/main.rs crates/imager/src/build.rs
git commit -m "feat(imager): stage Pi GPU firmware + DTB + generated config.txt/cmdline.txt"
```

---

### Task 5: machined mount_boot vfat fallback (MBR /boot)

**Files:**
- Modify: `crates/machined/src/imageboot.rs` (`mount_boot` fallback), and `crates/block` if a partition/fsprobe enumeration helper is needed

READ `crates/machined/src/imageboot.rs` `mount_boot` + `crates/block/src/{lib,sysfs,fsprobe}.rs` FIRST. `mount_boot` today finds the GPT `partition_label == "EFI"` volume via `block.list_volumes()`. On the Pi's MBR SD there is no GPT label, so it finds nothing. Add a fallback: if no EFI-labeled volume is found, mount the first **vfat** partition discovered (by fs-type probe), at `/boot`. The GPT path is tried FIRST and is unchanged — x86/aarch64 are unaffected.

- [ ] **Step 1: Failing test** (imageboot.rs tests, using the fake block backend)

```rust
#[tokio::test]
async fn mount_boot_falls_back_to_vfat_when_no_efi_label() {
    // An MBR disk: a partition with fs_type vfat but NO GPT "EFI" label.
    let platform = Arc::new(FakePlatform::new());
    let block = /* FakeBlockBackend seeded with a volume: partition_label="" (or non-EFI),
                   fs_type=Some("vfat"), device "/dev/mmcblk0p1" */;
    mount_boot(&block, platform.as_ref()).await.unwrap();
    // /boot mounted from the vfat partition.
    assert!(platform.mounts().iter().any(|m| m.target == "/boot" && m.source == "/dev/mmcblk0p1" && m.fstype == "vfat"));
}

#[tokio::test]
async fn mount_boot_prefers_efi_label_over_vfat_fallback() {
    // A GPT disk with an EFI-labeled partition AND another vfat partition:
    // the EFI label wins (x86/aarch64 path unchanged).
    // seed: vol A label="EFI" device=/dev/vda1; vol B label="" fs=vfat device=/dev/vda9
    // assert /boot mounts /dev/vda1 (the EFI one), not the fallback.
}
```

Adapt to `FakeBlockBackend`'s seeding API (read `crates/block/src/fake.rs` — how `VolumeInfo` with `partition_label` + `fs_type` is seeded) and `FakePlatform.mounts()`.

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined mount_boot_falls_back`
Expected: FAIL — no vfat fallback yet.

- [ ] **Step 3: Implement** — in `mount_boot`, after the existing EFI-label search yields no candidate, add the vfat fallback:

```rust
    // Existing: find the GPT EFI-labeled partition (x86/aarch64 GPT images).
    let efi = vols.iter().find(|v| v.partition_label == "EFI");
    // Fallback for MBR images (Raspberry Pi): the SD has no GPT label, so mount
    // the first vfat partition. Deterministic: sort by device.
    let target_vol = efi.or_else(|| {
        let mut vfat: Vec<_> = vols.iter()
            .filter(|v| v.fs_type.as_deref() == Some("vfat"))
            .collect();
        vfat.sort_by(|a, b| a.device.cmp(&b.device));
        vfat.into_iter().next()
    });
    let Some(v) = target_vol else { return Ok(()); };
    if platform.is_mounted("/boot")? { return Ok(()); }
    platform.mount(&MountSpec {
        source: v.device.clone(),
        target: "/boot".into(),
        fstype: "vfat".into(),
        flags: machined_platform::MS_RDONLY | machined_platform::MS_NOSUID | machined_platform::MS_NODEV,
        data: None,
    })?;
    info!("mounted boot partition {} at /boot", v.device);
```

Adjust to the real `VolumeInfo` field names (`partition_label`, `fs_type`, `device`) and the existing function structure (it already lists volumes + the >1-EFI warning — keep that for the EFI path). IMPORTANT: the block backend must report `fs_type` for MBR partitions too. Verify `SysfsBlock.list_volumes` fs-probes partitions regardless of GPT — if it ONLY surfaces GPT-discovered partitions (so an MBR partition never appears in `list_volumes`), extend it to also enumerate `/sys/block/*/` partitions and fs-probe them (the block crate has `fsprobe`). Report which path you took; add a block-crate test if you extend `list_volumes`.

- [ ] **Step 4: Run + workspace gate**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all`
Expected: the fallback tests pass; the existing EFI-label mount_boot tests (x86/aarch64) still pass (EFI is tried first).

- [ ] **Step 5: Commit**

```bash
git add crates/machined/src/imageboot.rs crates/block
git commit -m "feat(machined): mount_boot vfat fallback for MBR /boot (Raspberry Pi)"
```

---

### Task 6: Pi node config example

**Files:**
- Create: `examples/node-pi.yaml`

- [ ] **Step 1: Write the Pi node config** — no GPT provisioning (so no `install`), runtime enabled, network omitted (the Pi 3A+ has no wired NIC by default — serial verification; add a wired iface only if a USB-ethernet adapter is used):

```yaml
# Raspberry Pi 3A+ node. The Pi 3A+ has no built-in Ethernet (WiFi/BT only,
# which machined doesn't configure), so this config brings up no network by
# default — verify over the serial console. If you attach a USB-Ethernet
# adapter, add a `network.interfaces` entry for it (it appears as a wired NIC).
#
# No `install:` block: the SD card uses an MBR single FAT (Pi 3 firmware reads
# MBR, not GPT), so machined's GPT-based STATE/EPHEMERAL provisioning does not
# run on the Pi — the node boots from the initramfs + the FAT /boot.
machine:
  hostname: node-pi
  runtime:
    disabled: false
    binary: /boot/bin/containerd
```

- [ ] **Step 2: Pin it against schema drift** — add to `crates/imager/src/build.rs` tests (next to `ci_example_config_parses`):

```rust
#[test]
fn pi_example_config_parses() {
    let yaml = include_str!("../../../examples/node-pi.yaml");
    machined_config::load_from_str(yaml).expect("node-pi.yaml parses");
}
```

- [ ] **Step 3: Run + commit**

Run: `cargo test -p machined-imager pi_example`
Expected: PASS.

```bash
git add examples/node-pi.yaml crates/imager/src/build.rs
git commit -m "feat(example): node-pi.yaml (Pi 3A+: no install/GPT, runtime enabled)"
```

---

### Task 7: CI build-only job (FAT-readback, no boot)

**Files:**
- Create: `scripts/build-test-aarch64-rpi.sh`
- Modify: `Makefile` (target), `.github/workflows/ci.yml` (job)

- [ ] **Step 1: Build-only verification script** — `scripts/build-test-aarch64-rpi.sh` (chmod +x): build the Pi image (no qemu) and assert the FAT carries the Pi boot files by reading the image back with `machined-imager`'s own FAT path — simplest: re-use the imager build + a tiny assertion via Python or `mtools`-free readback. Since the image is MBR, read the FAT region at LBA 2048:

```bash
#!/usr/bin/env bash
# Build the aarch64-rpi (Pi 3A+) image and assert the FAT carries the Pi boot
# files. NO boot — qemu can't emulate Pi firmware; the node is verified on real
# hardware. This proves the image builds + stages correctly.
set -euo pipefail
cd "$(dirname "$0")/.."
TARGET_DIR=${CARGO_TARGET_DIR:-target}
WORK=target/boot-test
IMG=$WORK/machined-aarch64-rpi.img
MACHINED=$TARGET_DIR/aarch64-unknown-linux-musl/release/machined
IMAGER=$TARGET_DIR/release/machined-imager
rm -rf "$WORK"; mkdir -p "$WORK"
[ -x "$MACHINED" ] || { echo "FATAL: $MACHINED missing — run make dist-aarch64"; exit 2; }

"$IMAGER" gen-pki --out "$WORK/pki"
"$IMAGER" build --arch aarch64-rpi --machined "$MACHINED" \
  --config examples/node-pi.yaml --pki-dir "$WORK/pki" \
  --out "$IMG" --cache target/imager-cache

# Assert MBR (0x55AA, partition type 0x0C) + the FAT carries the Pi boot files.
python3 - "$IMG" <<'PY'
import sys, struct
raw = open(sys.argv[1], "rb").read(512)
assert raw[510:512] == b"\x55\xAA", "no MBR signature"
assert raw[446] == 0x80 and raw[446+4] == 0x0C, "partition 1 not bootable FAT32-LBA"
lba = struct.unpack("<I", raw[446+8:446+12])[0]
assert lba == 2048, f"FAT not at LBA 2048: {lba}"
print("MBR OK: bootable FAT32 primary at LBA 2048")
PY

# Mount the FAT (loopback offset) and check the boot files, if losetup/mount are
# available; else assert via the imager's --emit-boot is not applicable (MBR).
# Minimal, tool-light check: scan the raw image for the expected filenames in
# the FAT directory region (FAT32 8.3 + LFN store the names as bytes).
for f in CONFIG~1 CMDLINE BOOTCODE START VMLINUZ INITRAMF BCM2837; do
  grep -aqi "$(echo "$f" | cut -c1-6)" "$IMG" || { echo "FAT missing token: $f"; exit 1; }
done
echo "aarch64-rpi image built + Pi boot files present: BUILD CHECK PASSED"
```

NOTE: the raw `grep` for 8.3 tokens is a coarse but tool-light smoke check (no mount/losetup needed in the CI container). If `python3`/`losetup` are unavailable, keep the MBR-bytes Python check (python3 is in the CI image's base) and the grep tokens. If you prefer a robust readback, mount via `losetup -o $((2048*512))` + `mount -t vfat` and `ls` — but that needs privileges the container may lack; the byte/grep check is sufficient to prove staging.

- [ ] **Step 2: Makefile target** — add to `.PHONY` and after `boot-test-aarch64`:

```makefile
# Build the aarch64-rpi (Pi 3A+) image and verify its FAT — no boot (manual Pi).
build-image-aarch64-rpi: dist-aarch64
	cargo build --release -p machined-imager -p machinectl
	./scripts/build-test-aarch64-rpi.sh
```

- [ ] **Step 3: Run locally in the CI image** (no boot — fast):

```bash
docker run --rm -v "$PWD":/work -w /work machined-ci:local bash -c 'make build-image-aarch64-rpi' 2>&1 | tail -20
```
Expected: `BUILD CHECK PASSED` — the Pi image builds (downloads linux-rpi + firmware apks, stages blobs/DTB/config.txt), MBR is correct, FAT carries the boot files. Fix any staging gap that surfaces (a missing blob = a Task-4 issue → report it).

- [ ] **Step 4: CI job** — add to `.github/workflows/ci.yml` (clone the boot-test-aarch64 job, drop qemu — just `make build-image-aarch64-rpi`, 15-min timeout):

```yaml
  build-image-aarch64-rpi:
    runs-on: ubuntu-latest
    needs: check
    timeout-minutes: 20
    permissions:
      contents: read
      packages: read
    container:
      image: ghcr.io/indyjonesnl/machined-ci:latest
      credentials:
        username: ${{ github.actor }}
        password: ${{ secrets.GITHUB_TOKEN }}
    steps:
      - uses: actions/checkout@v4
      - uses: Swatinem/rust-cache@v2
      - name: cache imager artifacts
        uses: actions/cache@v4
        with:
          path: target/imager-cache
          key: imager-artifacts-${{ hashFiles('crates/imager/artifacts.toml') }}
      - name: build Pi 3A+ image (no boot)
        run: make build-image-aarch64-rpi
```

Validate YAML.

- [ ] **Step 5: Commit**

```bash
git add scripts/build-test-aarch64-rpi.sh Makefile .github/workflows/ci.yml
git commit -m "ci: build-only aarch64-rpi image job (FAT-readback, no boot)"
```

---

### Task 8: Manual Pi 3A+ checklist + docs + finish

**Files:**
- Create: `docs/raspberry-pi-3a-plus.md`
- Modify: `README.md` (status table + a pointer)

- [ ] **Step 1: Write the hardware checklist** — `docs/raspberry-pi-3a-plus.md`:

```markdown
# Booting machined on a Raspberry Pi 3A+

machined-rs builds a Pi 3A+ SD-card image with `machined-imager build --arch
aarch64-rpi`. CI builds + validates the image, but cannot boot it (QEMU doesn't
emulate the Pi VideoCore firmware) — verify on real hardware over serial.

## Build + flash
1. `make dist-aarch64 && cargo build --release -p machined-imager`
2. `target/release/machined-imager gen-pki --out /tmp/pki`
3. `target/release/machined-imager build --arch aarch64-rpi \
     --machined target/aarch64-unknown-linux-musl/release/machined \
     --config examples/node-pi.yaml --pki-dir /tmp/pki \
     --out machined-pi.img`
4. Flash: `sudo dd if=machined-pi.img of=/dev/sdX bs=4M conv=fsync` (X = your SD reader).

## Serial console
The Pi 3A+ has no Ethernet — verify over serial. Wire a 3.3 V USB-UART to the
GPIO header: GND→pin 6, TX→pin 8 (GPIO14), RX→pin 10 (GPIO15). `config.txt` sets
`enable_uart=1` + `dtoverlay=disable-bt` (PL011 on the header). Open at
**115200 8N1** (`screen /dev/ttyUSB0 115200` or `picocom -b 115200 /dev/ttyUSB0`).

## What you should see
Power on; the GPU firmware loads `bootcode.bin → start.elf`, then the kernel +
initramfs. On serial:
- the kernel boots (Linux 6.12.13-...-rpi)
- `machined starting (pid 1)`
- `mounted boot partition /dev/mmcblk0p1 at /boot` (the vfat fallback)
- `seeded PKI from /boot/pki`
- `management API listening on 0.0.0.0:50000`
- `containerd successfully booted` (from /boot/bin)
- `RuntimeStatus ready=true` shortly after (via `machinectl` if networked)

No STATE/EPHEMERAL provisioning runs (the Pi image is MBR, not GPT — by design).

## Optional: machinectl over the network
Plug a USB-Ethernet adapter into the Pi's USB port; it appears as a wired NIC.
Add a `network.interfaces` entry to `node-pi.yaml` for it (static IP), rebuild,
reflash. Then from your workstation:
`machinectl --bundle /tmp/pki/machinectl --endpoint https://<pi-ip>:50000 version`.

## Known hardware-gated items (report if they differ)
- **MBR vs GPT:** the image is MBR (Pi 3 firmware reads MBR). If a future Pi
  needs GPT, that's a separate change.
- **DTB:** `config.txt` sets `device_tree=bcm2837-rpi-3-a-plus.dtb` explicitly.
  If your board's firmware prefers auto-select, dropping that line also works.
- **Console:** `console=serial0,115200` maps to the PL011 with `disable-bt`. If
  the serial is silent, try `console=ttyAMA0,115200` or `console=ttyS0,115200`.
- **No persistence:** PKI is seeded from /boot each boot; containerd's root is on
  the initramfs (ephemeral). Persistent volumes on Pi = a future milestone.
```

- [ ] **Step 2: README status** — flip the aarch64/Pi row to note Pi 3A+ image support (build + manual-verify) and link `docs/raspberry-pi-3a-plus.md`. Add `make build-image-aarch64-rpi` under Build & test.

- [ ] **Step 3: Gates + finish**

Run: `make pre-commit`
Expected: clean. Then superpowers:finishing-a-development-branch. **Two-PR sequencing** (the CI image is unchanged this milestone — no qemu/toolchain additions — so a single PR suffices UNLESS the build-only job needs a tool not in the image; it uses only python3 + the existing rust/imager, which are present, so **one PR is fine**). Merge to main, confirm CI green (check + boot-test + boot-test-aarch64 + build-image-aarch64-rpi).

```bash
git add docs/raspberry-pi-3a-plus.md README.md
git commit -m "docs(pi): Raspberry Pi 3A+ boot checklist + README status"
```

---

## Verification (end-to-end)

1. `cargo test --workspace` green: ArchConfig table, aarch64-rpi manifest parse, MBR writer (byte-level + FAT readback), Pi firmware staging, mount_boot vfat fallback, node-pi parse.
2. `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all --check` clean.
3. CI `build-image-aarch64-rpi` builds the Pi image (downloads linux-rpi + firmware apks, stages blobs/DTB/config.txt, writes MBR+FAT) and the FAT-readback check passes. (No boot — manual.)
4. **Manual (operator):** flash to a Pi 3A+ SD, serial console shows the node boot through machined → /boot mount → PKI seed → containerd. Documented in `docs/raspberry-pi-3a-plus.md`.

## Known gaps / deferred (documented)

- **No automated Pi boot** — qemu can't emulate the VideoCore firmware; the Pi boot is hardware-verified only. CI proves the image is well-formed (MBR + FAT contents), not that it boots.
- **No STATE/EPHEMERAL provisioning on Pi** — CompleteLayout is GPT-based; the MBR Pi image boots from initramfs + FAT only. Persistent volumes on Pi (MBR provisioning, or hybrid-MBR + machined GPT-preservation) = a future milestone if wanted.
- **Pi 3A+ has no Ethernet** — serial-primary verification; machinectl-over-network needs a USB-Ethernet adapter + a node-config NIC entry.
- **Hardware-gated unknowns** (DTB auto-select, exact console device, MBR boot itself) — listed in the checklist for the operator to confirm/report.
- **Pin freshness** — Alpine rolls `-rN` within v3.21; re-pin from the dir listing if a linux-rpi/bootloader apk 404s later.
