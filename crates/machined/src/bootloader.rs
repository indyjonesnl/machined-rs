//! Boot-slot selection for A/B disk upgrades. A BootloaderBackend abstracts the
//! per-platform bootloader: SdBootBackend (UEFI/systemd-boot) here; a future
//! PiBootBackend over autoboot.txt/tryboot. The upgrade flow is backend-agnostic.

use std::path::Path;

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
}
