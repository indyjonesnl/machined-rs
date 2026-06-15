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
    /// Partition table scheme. Consumed by the image writer (M7c-2 Task 3).
    pub scheme: PartScheme,
    /// True when this arch needs Raspberry Pi firmware staging (M7c-2 Task 4).
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
        // Test-only vehicle: the qemu-virt kernel (boots on -M virt) but with the
        // Pi's MBR partition table instead of GPT. It exists so CI can exercise
        // machined's MBR partition read (the gpt-crate-fails -> sysfs-fallback
        // path) and the vfat /boot mount on a real kernel — the exact code path
        // the Raspberry Pi uses, which qemu's raspi3 SD model cannot exercise
        // (sdhost never exposes mmcblk0pN; see scripts/boot-test-aarch64-rpi.sh).
        // No real hardware ships this combo; it is purely an emulation-coverage
        // stand-in for the Pi's MBR boot disk.
        "aarch64-mbr" => ArchConfig {
            kernel_path: "boot/vmlinuz-virt",
            module_roots: crate::modules::VIRT_MODULES,
            scheme: PartScheme::Mbr,
            rpi_firmware: false,
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
    fn mbr_test_arch_uses_virt_kernel_with_mbr_scheme() {
        // The aarch64-mbr coverage vehicle: virt kernel (boots on -M virt) but
        // MBR scheme, so it exercises the Pi's MBR/vfat boot path under a machine
        // whose disk DOES expose partitions. No Pi firmware.
        let c = arch_config("aarch64-mbr").unwrap();
        assert_eq!(c.kernel_path, "boot/vmlinuz-virt");
        assert_eq!(c.scheme, PartScheme::Mbr);
        assert!(!c.rpi_firmware);
        assert_eq!(c.module_roots, crate::modules::VIRT_MODULES);
    }

    #[test]
    fn unknown_arch_is_none() {
        assert!(arch_config("riscv").is_none());
    }
}
