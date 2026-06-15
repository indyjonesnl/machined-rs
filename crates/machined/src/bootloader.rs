//! Boot-slot selection for A/B disk upgrades. A BootloaderBackend abstracts the
//! per-platform bootloader: SdBootBackend (UEFI/systemd-boot) here; a future
//! PiBootBackend over autoboot.txt/tryboot. The upgrade flow is backend-agnostic.

use anyhow::Context as _;
use machined_platform::Platform;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
    /// Lowercase id used in loader.conf / cmdline ("a"/"b").
    pub fn id(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }
    /// Uppercase ESP subdir ("A"/"B").
    pub fn dir(self) -> &'static str {
        match self {
            Slot::A => "A",
            Slot::B => "B",
        }
    }
}

/// Parse `machined.slot=a|b` out of a kernel command line. Absent/garbage → A
/// (the imager always writes the token; default A keeps a hand-booted node sane).
pub fn parse_slot(cmdline: &str) -> Slot {
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("machined.slot=") {
            if v == "b" {
                return Slot::B;
            }
            return Slot::A;
        }
    }
    Slot::A
}

/// Backend over the on-disk bootloader. `esp` is the mounted ESP root (/boot).
pub trait BootloaderBackend {
    /// Which slot the running kernel booted from.
    fn current_slot(&self) -> Slot;
    /// Write kernel+initramfs into the inactive slot dir on the ESP; return it.
    fn stage_inactive(&self, kernel: &Path, initrd: &Path) -> anyhow::Result<Slot>;
    /// Flip the boot pointer (loader.conf default) to `slot`.
    fn set_active(&self, slot: Slot) -> anyhow::Result<()>;
}

/// systemd-boot backend. `esp` is the mounted ESP root (machined mounts the EFI
/// partition at /boot). Writes slot dirs + rewrites loader.conf's `default`.
pub struct SdBootBackend {
    esp: std::path::PathBuf,
    platform: Arc<dyn Platform>,
    current: Slot,
}

impl SdBootBackend {
    /// `esp` = the ESP mount point (/boot). `cmdline` = /proc/cmdline contents.
    pub fn new(
        esp: impl Into<std::path::PathBuf>,
        platform: Arc<dyn Platform>,
        cmdline: &str,
    ) -> Self {
        Self {
            esp: esp.into(),
            platform,
            current: parse_slot(cmdline),
        }
    }
}

impl BootloaderBackend for SdBootBackend {
    fn current_slot(&self) -> Slot {
        self.current
    }

    fn stage_inactive(&self, kernel: &Path, initrd: &Path) -> anyhow::Result<Slot> {
        let slot = self.current.other();
        let esp = self.esp.to_string_lossy().to_string();
        // The ESP is mounted ro at runtime; remount rw for the staging window.
        self.platform
            .remount_rw(&esp)
            .map_err(|e| anyhow::anyhow!("remount {esp} rw: {e}"))?;
        let res = (|| -> anyhow::Result<()> {
            let dir = self.esp.join(slot.dir());
            std::fs::create_dir_all(&dir)?;
            std::fs::copy(kernel, dir.join("vmlinuz"))?;
            std::fs::copy(initrd, dir.join("initramfs.img"))?;
            if let Ok(d) = std::fs::File::open(&dir) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        let _ = self.platform.remount_ro(&esp);
        res?;
        Ok(slot)
    }

    fn set_active(&self, slot: Slot) -> anyhow::Result<()> {
        let esp = self.esp.to_string_lossy().to_string();
        let conf = self.esp.join("loader/loader.conf");
        self.platform
            .remount_rw(&esp)
            .map_err(|e| anyhow::anyhow!("remount {esp} rw: {e}"))?;
        let res = std::fs::write(&conf, format!("default {}\ntimeout 0\n", slot.id()))
            .map_err(anyhow::Error::from);
        let _ = self.platform.remount_ro(&esp);
        res.with_context(|| format!("write {}", conf.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slot_reads_token_or_defaults_a() {
        assert_eq!(parse_slot("console=ttyS0 machined.slot=b"), Slot::B);
        assert_eq!(parse_slot("machined.slot=a console=ttyS0"), Slot::A);
        assert_eq!(parse_slot("console=ttyS0"), Slot::A); // absent → A
        assert_eq!(parse_slot("machined.slot=x"), Slot::A); // garbage → A
    }

    #[test]
    fn slot_other_and_ids() {
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
        assert_eq!((Slot::A.id(), Slot::A.dir()), ("a", "A"));
        assert_eq!((Slot::B.id(), Slot::B.dir()), ("b", "B"));
    }

    #[test]
    fn sdboot_stages_inactive_and_flips_pointer() {
        use machined_platform::FakePlatform;
        let dir = tempfile::tempdir().unwrap();
        let esp = dir.path().join("boot");
        std::fs::create_dir_all(esp.join("loader")).unwrap();
        std::fs::write(esp.join("loader/loader.conf"), "default a\ntimeout 0\n").unwrap();
        std::fs::write(dir.path().join("vmlinuz"), b"K2").unwrap();
        std::fs::write(dir.path().join("initramfs.img"), b"I2").unwrap();

        let be = SdBootBackend::new(
            &esp,
            std::sync::Arc::new(FakePlatform::new()),
            "machined.slot=a",
        );
        let slot = be
            .stage_inactive(
                &dir.path().join("vmlinuz"),
                &dir.path().join("initramfs.img"),
            )
            .unwrap();
        assert_eq!(slot, Slot::B);
        assert_eq!(std::fs::read(esp.join("B/vmlinuz")).unwrap(), b"K2");
        assert_eq!(std::fs::read(esp.join("B/initramfs.img")).unwrap(), b"I2");

        be.set_active(Slot::B).unwrap();
        assert_eq!(
            std::fs::read_to_string(esp.join("loader/loader.conf")).unwrap(),
            "default b\ntimeout 0\n"
        );
    }
}
