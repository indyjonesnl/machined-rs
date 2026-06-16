//! Boot-slot selection for A/B disk upgrades. A BootloaderBackend abstracts the
//! per-platform bootloader: SdBootBackend (UEFI/systemd-boot) and PiBootBackend
//! (Raspberry Pi, config.txt os_prefix). The upgrade flow is backend-agnostic.

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

/// Write kernel+initramfs into `slot`'s dir on the ESP, inside an rw remount
/// window (the ESP is mounted ro at runtime), fsync the dir, and re-seal ro.
/// Shared by SdBootBackend and PiBootBackend (their slot dirs are identical;
/// only the boot-pointer file differs). Returns the staging error (the ro
/// re-seal is best-effort) so the caller can still observe a failed write.
fn write_inactive_slot(
    esp: &Path,
    platform: &dyn Platform,
    slot: Slot,
    kernel: &Path,
    initrd: &Path,
) -> anyhow::Result<()> {
    let esp_s = esp.to_string_lossy().to_string();
    platform
        .remount_rw(&esp_s)
        .map_err(|e| anyhow::anyhow!("remount {esp_s} rw: {e}"))?;
    let res = (|| -> anyhow::Result<()> {
        let dir = esp.join(slot.dir());
        std::fs::create_dir_all(&dir)?;
        std::fs::copy(kernel, dir.join("vmlinuz"))?;
        std::fs::copy(initrd, dir.join("initramfs.img"))?;
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if let Err(e) = platform.remount_ro(&esp_s) {
        tracing::warn!("remount {esp_s} ro failed: {e}");
    }
    res
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
        write_inactive_slot(&self.esp, self.platform.as_ref(), slot, kernel, initrd)?;
        Ok(slot)
    }

    fn set_active(&self, slot: Slot) -> anyhow::Result<()> {
        let esp = self.esp.to_string_lossy().to_string();
        let conf = self.esp.join("loader/loader.conf");
        self.platform
            .remount_rw(&esp)
            .map_err(|e| anyhow::anyhow!("remount {esp} rw: {e}"))?;
        let res = (|| -> std::io::Result<()> {
            std::fs::write(&conf, format!("default {}\ntimeout 0\n", slot.id()))?;
            // loader.conf is THE A/B boot pointer: a torn/empty write that
            // survives a power cut leaves systemd-boot unable to parse either
            // slot. fsync the file AND the loader/ dir (best-effort) so the
            // flip is durable before we re-seal the ESP ro.
            let _ = std::fs::File::open(&conf).and_then(|f| f.sync_all());
            if let Some(parent) = conf.parent() {
                let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
            }
            Ok(())
        })()
        .map_err(anyhow::Error::from);
        if let Err(e) = self.platform.remount_ro(&esp) {
            tracing::warn!("remount {esp} ro failed: {e}");
        }
        res.with_context(|| format!("write {}", conf.display()))
    }
}

/// Replace the value of config.txt's `os_prefix=` line with `<slot>/`, preserving
/// every other line and order. Errors if there is no os_prefix= line to flip
/// (the imager always writes one). The output always ends with a newline.
fn rewrite_os_prefix(config: &str, slot: Slot) -> anyhow::Result<String> {
    let mut found = false;
    let mut out = String::with_capacity(config.len() + 8);
    for line in config.lines() {
        if line.trim_start().starts_with("os_prefix=") {
            out.push_str(&format!("os_prefix={}/", slot.dir()));
            found = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    anyhow::ensure!(found, "config.txt has no os_prefix= line to flip");
    Ok(out)
}

/// Raspberry Pi backend. The VideoCore firmware reads /boot/config.txt and an
/// `os_prefix=<slot>/` directive selects a self-contained slot dir (/A or /B).
/// stage_inactive writes the inactive slot's kernel+initramfs (the dtb,
/// cmdline.txt and overlays are scaffolded into the slot by the imager);
/// set_active flips config.txt's os_prefix line.
pub struct PiBootBackend {
    esp: std::path::PathBuf,
    platform: Arc<dyn Platform>,
    current: Slot,
}

impl PiBootBackend {
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

impl BootloaderBackend for PiBootBackend {
    fn current_slot(&self) -> Slot {
        self.current
    }

    fn stage_inactive(&self, kernel: &Path, initrd: &Path) -> anyhow::Result<Slot> {
        let slot = self.current.other();
        write_inactive_slot(&self.esp, self.platform.as_ref(), slot, kernel, initrd)?;
        Ok(slot)
    }

    fn set_active(&self, slot: Slot) -> anyhow::Result<()> {
        let esp_s = self.esp.to_string_lossy().to_string();
        let conf = self.esp.join("config.txt");
        self.platform
            .remount_rw(&esp_s)
            .map_err(|e| anyhow::anyhow!("remount {esp_s} rw: {e}"))?;
        let res = (|| -> anyhow::Result<()> {
            let content = std::fs::read_to_string(&conf)
                .with_context(|| format!("read {}", conf.display()))?;
            let new = rewrite_os_prefix(&content, slot)?;
            std::fs::write(&conf, &new).with_context(|| format!("write {}", conf.display()))?;
            // config.txt is THE Pi boot pointer: fsync the file + the dir so the
            // os_prefix flip survives a power cut before we re-seal the ESP ro.
            let _ = std::fs::File::open(&conf).and_then(|f| f.sync_all());
            if let Some(parent) = conf.parent() {
                let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
            }
            Ok(())
        })();
        if let Err(e) = self.platform.remount_ro(&esp_s) {
            tracing::warn!("remount {esp_s} ro failed: {e}");
        }
        res
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

        let fake = std::sync::Arc::new(FakePlatform::new());
        let be = SdBootBackend::new(&esp, fake.clone(), "machined.slot=a");
        let esp_s = esp.to_string_lossy().to_string();
        let slot = be
            .stage_inactive(
                &dir.path().join("vmlinuz"),
                &dir.path().join("initramfs.img"),
            )
            .unwrap();
        assert_eq!(slot, Slot::B);
        assert_eq!(std::fs::read(esp.join("B/vmlinuz")).unwrap(), b"K2");
        assert_eq!(std::fs::read(esp.join("B/initramfs.img")).unwrap(), b"I2");
        // The staging window must remount the ESP rw then re-seal it ro.
        assert_eq!(
            fake.remounts(),
            vec![(esp_s.clone(), true), (esp_s.clone(), false)]
        );

        be.set_active(Slot::B).unwrap();
        assert_eq!(
            std::fs::read_to_string(esp.join("loader/loader.conf")).unwrap(),
            "default b\ntimeout 0\n"
        );
        // set_active opens a second rw/ro window: ESP must end up ro again.
        assert_eq!(
            fake.remounts(),
            vec![
                (esp_s.clone(), true),
                (esp_s.clone(), false),
                (esp_s.clone(), true),
                (esp_s.clone(), false),
            ]
        );
    }

    #[test]
    fn rewrite_os_prefix_flips_only_that_line() {
        let cfg = "arm_64bit=1\nos_prefix=A/\nkernel=vmlinuz\ndtoverlay=disable-bt\n";
        let out = rewrite_os_prefix(cfg, Slot::B).unwrap();
        assert_eq!(
            out,
            "arm_64bit=1\nos_prefix=B/\nkernel=vmlinuz\ndtoverlay=disable-bt\n"
        );
        assert!(
            out.contains("arm_64bit=1")
                && out.contains("kernel=vmlinuz")
                && out.contains("dtoverlay=disable-bt")
        );
        assert!(rewrite_os_prefix("arm_64bit=1\nkernel=vmlinuz\n", Slot::B).is_err());
    }

    #[test]
    fn piboot_stages_inactive_and_flips_os_prefix() {
        use machined_platform::FakePlatform;
        let dir = tempfile::tempdir().unwrap();
        let esp = dir.path().join("boot");
        std::fs::create_dir_all(&esp).unwrap();
        std::fs::write(
            esp.join("config.txt"),
            "arm_64bit=1\nos_prefix=A/\nkernel=vmlinuz\ndtoverlay=disable-bt\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("vmlinuz"), b"K2").unwrap();
        std::fs::write(dir.path().join("initramfs.img"), b"I2").unwrap();

        let fake = std::sync::Arc::new(FakePlatform::new());
        let be = PiBootBackend::new(&esp, fake.clone(), "machined.slot=a");
        let esp_s = esp.to_string_lossy().to_string();
        let slot = be
            .stage_inactive(
                &dir.path().join("vmlinuz"),
                &dir.path().join("initramfs.img"),
            )
            .unwrap();
        assert_eq!(slot, Slot::B);
        assert_eq!(std::fs::read(esp.join("B/vmlinuz")).unwrap(), b"K2");
        assert_eq!(std::fs::read(esp.join("B/initramfs.img")).unwrap(), b"I2");
        assert_eq!(
            fake.remounts(),
            vec![(esp_s.clone(), true), (esp_s.clone(), false)]
        );

        be.set_active(Slot::B).unwrap();
        assert_eq!(
            std::fs::read_to_string(esp.join("config.txt")).unwrap(),
            "arm_64bit=1\nos_prefix=B/\nkernel=vmlinuz\ndtoverlay=disable-bt\n"
        );
        assert_eq!(
            fake.remounts(),
            vec![
                (esp_s.clone(), true),
                (esp_s.clone(), false),
                (esp_s.clone(), true),
                (esp_s.clone(), false),
            ]
        );
    }
}
