//! Raspberry Pi firmware staging onto the FAT boot partition: copy the GPU
//! blobs (bootcode.bin, start.elf, fixup.dat) + the Pi 3A+ DTB from the
//! extracted rootfs, and generate config.txt / cmdline.txt.

use anyhow::Context;
use std::path::Path;

/// The GPU firmware blobs a bcm2837 (Pi 3) boot needs, found under rootfs/boot/.
const PI3_BLOBS: &[&str] = &["bootcode.bin", "start.elf", "fixup.dat"];
/// The Pi 3A+ device tree. Staged into the FAT for the real firmware boot, and
/// also emitted beside vmlinuz (`--emit-boot`) so the qemu raspi3ap boot-test
/// can pass it via `-kernel … -dtb`: qemu's auto-generated raspi3ap dtb is
/// incompatible with this Alpine linux-rpi kernel (it hangs in head.S before any
/// serial), whereas the real board dtb boots cleanly.
pub(crate) const PI3_DTB: &str = "bcm2837-rpi-3-a-plus.dtb";

/// Overlays referenced by config.txt's dtoverlay= lines. os_prefix prepends to
/// overlay paths too, so each slot needs its own copy — most importantly
/// disable-bt, which puts the PL011 on the GPIO header (the serial console).
const PI3_OVERLAYS: &[&str] = &["disable-bt.dtbo"];

/// config.txt for a 64-bit Pi 3A+ booting kernel + initramfs (headless node).
pub fn config_txt() -> &'static str {
    "arm_64bit=1\n\
     os_prefix=A/\n\
     kernel=vmlinuz\n\
     initramfs initramfs.img followkernel\n\
     gpu_mem=16\n\
     enable_uart=1\n\
     dtoverlay=disable-bt\n\
     device_tree=bcm2837-rpi-3-a-plus.dtb\n"
}

/// cmdline.txt for a slot — the base console args plus the machined.slot token
/// (PiBootBackend reads it from /proc/cmdline via parse_slot). serial0 maps to
/// the PL011 on the header (disable-bt). machined is /init, so no root=.
pub fn cmdline_txt_for(slot: &str) -> String {
    format!("console=serial0,115200 console=tty1 machined.slot={slot}\n")
}

/// Stage Pi firmware: copy blobs + DTB from rootfs/boot into staging, write
/// config.txt + cmdline.txt. The kernel is already staged as `vmlinuz` by the
/// generic path (config.txt names it).
///
/// # Errors
/// Fails if a required blob/DTB is missing from the rootfs or on I/O error.
pub fn stage_pi_firmware(rootfs: &Path, staging: &Path) -> anyhow::Result<()> {
    let boot = rootfs.join("boot");
    // Firmware blobs + config.txt at the FAT root (read before os_prefix applies).
    for f in PI3_BLOBS {
        let src = boot.join(f);
        anyhow::ensure!(
            src.exists(),
            "Pi firmware blob {f} missing (raspberrypi-bootloader apks)"
        );
        std::fs::copy(&src, staging.join(f)).with_context(|| format!("stage {f}"))?;
    }
    std::fs::write(staging.join("config.txt"), config_txt()).context("write config.txt")?;

    // Each slot dir is self-contained: dtb + overlays + cmdline.txt (os_prefix
    // prepends to ALL of these). The kernel+initramfs are moved into /A later
    // (move_kernel_to_slot_a); /B's kernel is staged by the first upgrade.
    let dtb_src = boot.join(PI3_DTB);
    anyhow::ensure!(
        dtb_src.exists(),
        "Pi 3A+ DTB {PI3_DTB} missing (linux-rpi apk)"
    );
    for (dir, id) in [("A", "a"), ("B", "b")] {
        let slot = staging.join(dir);
        let overlays = slot.join("overlays");
        std::fs::create_dir_all(&overlays)
            .with_context(|| format!("create slot dir {}", overlays.display()))?;
        std::fs::copy(&dtb_src, slot.join(PI3_DTB))
            .with_context(|| format!("stage {PI3_DTB} into {dir}"))?;
        for ovl in PI3_OVERLAYS {
            let src = boot.join("overlays").join(ovl);
            anyhow::ensure!(src.exists(), "Pi overlay {ovl} missing (linux-rpi apk)");
            std::fs::copy(&src, overlays.join(ovl))
                .with_context(|| format!("stage overlay {ovl} into {dir}"))?;
        }
        std::fs::write(slot.join("cmdline.txt"), cmdline_txt_for(id))
            .with_context(|| format!("write {dir}/cmdline.txt"))?;
    }
    Ok(())
}

/// Move the staged kernel+initramfs from the staging root into slot A. Called
/// AFTER the generic path writes staging/{vmlinuz,initramfs.img} (build.rs),
/// mirroring sdboot::assemble for the GPT arches.
///
/// # Errors
/// Fails on any directory-creation or rename I/O error.
pub fn move_kernel_to_slot_a(staging: &Path) -> anyhow::Result<()> {
    let a = staging.join("A");
    std::fs::create_dir_all(&a).with_context(|| format!("create {}", a.display()))?;
    for f in ["vmlinuz", "initramfs.img"] {
        std::fs::rename(staging.join(f), a.join(f))
            .with_context(|| format!("move {f} into slot A"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_has_os_prefix_and_essentials() {
        let c = config_txt();
        assert!(c.contains("os_prefix=A/"), "{c}");
        assert!(c.contains("arm_64bit=1") && c.contains("kernel=vmlinuz"));
        assert!(c.contains("initramfs initramfs.img followkernel"));
        assert!(
            c.contains("dtoverlay=disable-bt")
                && c.contains("device_tree=bcm2837-rpi-3-a-plus.dtb")
        );
        assert!(cmdline_txt_for("a").contains("console=serial0,115200"));
        assert!(cmdline_txt_for("a").contains("machined.slot=a"));
        assert!(cmdline_txt_for("b").contains("machined.slot=b"));
    }

    #[test]
    fn scaffolds_both_slots_with_dtb_overlay_cmdline_and_moves_kernel() {
        let dir = tempfile::tempdir().unwrap();
        let (rootfs, staging) = (dir.path().join("rootfs"), dir.path().join("staging"));
        std::fs::create_dir_all(rootfs.join("boot/overlays")).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        for f in PI3_BLOBS {
            std::fs::write(rootfs.join("boot").join(f), f.as_bytes()).unwrap();
        }
        std::fs::write(rootfs.join("boot").join(PI3_DTB), b"dtb").unwrap();
        std::fs::write(rootfs.join("boot/overlays/disable-bt.dtbo"), b"ovl").unwrap();

        stage_pi_firmware(&rootfs, &staging).unwrap();
        for f in PI3_BLOBS {
            assert!(staging.join(f).exists());
        }
        assert!(std::fs::read_to_string(staging.join("config.txt"))
            .unwrap()
            .contains("os_prefix=A/"));
        for (d, id) in [("A", "a"), ("B", "b")] {
            assert_eq!(
                std::fs::read(staging.join(d).join(PI3_DTB)).unwrap(),
                b"dtb"
            );
            assert_eq!(
                std::fs::read(staging.join(d).join("overlays/disable-bt.dtbo")).unwrap(),
                b"ovl"
            );
            let cl = std::fs::read_to_string(staging.join(d).join("cmdline.txt")).unwrap();
            assert!(cl.contains(&format!("machined.slot={id}")), "{cl}");
        }
        std::fs::write(staging.join("vmlinuz"), b"K").unwrap();
        std::fs::write(staging.join("initramfs.img"), b"I").unwrap();
        move_kernel_to_slot_a(&staging).unwrap();
        assert_eq!(std::fs::read(staging.join("A/vmlinuz")).unwrap(), b"K");
        assert_eq!(
            std::fs::read(staging.join("A/initramfs.img")).unwrap(),
            b"I"
        );
        assert!(!staging.join("vmlinuz").exists());
        assert!(!staging.join("B/vmlinuz").exists());
    }

    #[test]
    fn missing_blob_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (rootfs, staging) = (dir.path().join("rootfs"), dir.path().join("staging"));
        std::fs::create_dir_all(rootfs.join("boot")).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        let err = stage_pi_firmware(&rootfs, &staging).unwrap_err();
        assert!(err.to_string().contains("bootcode.bin"), "{err}");
    }
}
