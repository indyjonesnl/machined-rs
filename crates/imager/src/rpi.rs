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
        anyhow::ensure!(
            src.exists(),
            "Pi firmware blob {f} missing (raspberrypi-bootloader apks)"
        );
        std::fs::copy(&src, staging.join(f)).with_context(|| format!("stage {f}"))?;
    }
    let dtb_src = boot.join(PI3_DTB);
    anyhow::ensure!(
        dtb_src.exists(),
        "Pi 3A+ DTB {PI3_DTB} missing (linux-rpi apk)"
    );
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
        assert!(c.contains("dtoverlay=disable-bt"));
        assert!(c.contains("device_tree=bcm2837-rpi-3-a-plus.dtb"));
        assert!(cmdline_txt().contains("console=serial0,115200"));
    }

    #[test]
    fn stages_blobs_dtb_and_generated_configs() {
        let dir = tempfile::tempdir().unwrap();
        let (rootfs, staging) = (dir.path().join("rootfs"), dir.path().join("staging"));
        std::fs::create_dir_all(rootfs.join("boot")).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        for f in PI3_BLOBS {
            std::fs::write(rootfs.join("boot").join(f), f.as_bytes()).unwrap();
        }
        std::fs::write(rootfs.join("boot").join(PI3_DTB), b"dtb").unwrap();

        stage_pi_firmware(&rootfs, &staging).unwrap();

        for f in PI3_BLOBS {
            assert_eq!(std::fs::read(staging.join(f)).unwrap(), f.as_bytes());
        }
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
        let err = stage_pi_firmware(&rootfs, &staging).unwrap_err();
        assert!(err.to_string().contains("bootcode.bin"), "{err}");
    }
}
