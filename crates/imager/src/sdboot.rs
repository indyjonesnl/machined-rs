//! Assemble a systemd-boot A/B layout inside the imager's FAT staging tree.
//! The ESP gets: /loader/loader.conf (default=a), /loader/entries/{a,b}.conf,
//! and slot A populated at /A/{vmlinuz,initramfs.img}. Slot B is created by the
//! first on-device upgrade (machined), not here. The systemd-boot binary itself
//! is staged separately (build.rs, the sd-boot-efi artifact kind).

use anyhow::Context as _;
use std::path::Path;

/// loader.conf: pick slot A by default, no menu timeout (headless).
fn loader_conf() -> &'static str {
    "default a\ntimeout 0\n"
}

/// A type-1 boot entry for slot `slot` ("a"/"b"). `cmdline` is the kernel
/// command line WITHOUT the slot token; we append `machined.slot=<slot>` so the
/// running machined knows which slot it booted (bootloader.rs reads /proc/cmdline).
fn entry_conf(slot: &str, cmdline: &str) -> String {
    format!(
        "title machined ({slot})\n\
         linux /{up}/vmlinuz\n\
         initrd /{up}/initramfs.img\n\
         options {cmdline} machined.slot={slot}\n",
        up = slot.to_uppercase()
    )
}

/// Lay out systemd-boot A/B inside `staging`, moving the already-staged
/// `staging/vmlinuz` + `staging/initramfs.img` into `staging/A/`. `cmdline` is
/// the base kernel cmdline (e.g. "console=ttyS0").
pub fn assemble(staging: &Path, cmdline: &str) -> anyhow::Result<()> {
    // /A holds slot A's kernel+initramfs (moved from the staging root).
    let slot_a = staging.join("A");
    std::fs::create_dir_all(&slot_a).with_context(|| format!("create {}", slot_a.display()))?;
    for f in ["vmlinuz", "initramfs.img"] {
        std::fs::rename(staging.join(f), slot_a.join(f))
            .with_context(|| format!("move {f} into slot A"))?;
    }
    // /loader/loader.conf + entries.
    let entries = staging.join("loader/entries");
    std::fs::create_dir_all(&entries).with_context(|| format!("create {}", entries.display()))?;
    std::fs::write(staging.join("loader/loader.conf"), loader_conf())
        .context("write loader.conf")?;
    std::fs::write(entries.join("a.conf"), entry_conf("a", cmdline)).context("write a.conf")?;
    std::fs::write(entries.join("b.conf"), entry_conf("b", cmdline)).context("write b.conf")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_moves_kernel_and_writes_loader() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        std::fs::write(s.join("vmlinuz"), b"k").unwrap();
        std::fs::write(s.join("initramfs.img"), b"i").unwrap();

        assemble(s, "console=ttyS0").unwrap();

        assert_eq!(std::fs::read(s.join("A/vmlinuz")).unwrap(), b"k");
        assert_eq!(std::fs::read(s.join("A/initramfs.img")).unwrap(), b"i");
        assert!(!s.join("vmlinuz").exists());
        assert_eq!(
            std::fs::read_to_string(s.join("loader/loader.conf")).unwrap(),
            "default a\ntimeout 0\n"
        );
        let a = std::fs::read_to_string(s.join("loader/entries/a.conf")).unwrap();
        assert!(a.contains("linux /A/vmlinuz"), "{a}");
        assert!(a.contains("machined.slot=a"), "{a}");
        let b = std::fs::read_to_string(s.join("loader/entries/b.conf")).unwrap();
        assert!(
            b.contains("linux /B/vmlinuz") && b.contains("machined.slot=b"),
            "{b}"
        );
    }
}
