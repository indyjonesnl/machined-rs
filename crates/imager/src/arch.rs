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
