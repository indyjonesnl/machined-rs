//! Filesystem identification by magic bytes, for ext4, vfat, xfs, and swap.
//! Pure and root-free: callers pass the leading bytes of a partition device.

use crate::FsType;

/// Probed filesystem identity. `label`/`uuid` are populated where trivially
/// available (ext4); other filesystems report only the type in M2b-1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsProbe {
    pub fs_type: FsType,
    pub label: Option<String>,
    pub uuid: Option<String>,
}

/// Identify the filesystem from the leading bytes of a device (needs >= 4096
/// bytes to detect swap; less is fine for the others). Returns `None` for an
/// unrecognised filesystem.
pub fn probe_fs(buf: &[u8]) -> Option<FsProbe> {
    // ext2/3/4: superblock at byte 1024, s_magic 0xEF53 (LE) at offset 0x38.
    if buf.len() >= 1082 && buf[1080] == 0x53 && buf[1081] == 0xEF {
        let uuid = (buf.len() >= 1144).then(|| format_uuid(&buf[1128..1144]));
        let label = (buf.len() >= 1160)
            .then(|| read_cstr(&buf[1144..1160]))
            .flatten();
        return Some(FsProbe {
            fs_type: FsType::Ext4,
            label,
            uuid,
        });
    }
    // xfs: "XFSB" at byte 0.
    if buf.len() >= 4 && &buf[0..4] == b"XFSB" {
        return Some(FsProbe {
            fs_type: FsType::Xfs,
            label: None,
            uuid: None,
        });
    }
    // vfat: boot-sector signature 0x55AA at 510, FAT type string at 54 or 82.
    if buf.len() >= 512 && buf[510] == 0x55 && buf[511] == 0xAA {
        let fat32 = buf.len() >= 85 && &buf[82..85] == b"FAT";
        let fat16 = buf.len() >= 57 && &buf[54..57] == b"FAT";
        if fat32 || fat16 {
            return Some(FsProbe {
                fs_type: FsType::Vfat,
                label: None,
                uuid: None,
            });
        }
    }
    // swap: signature in the last 10 bytes of the first 4096-byte page.
    if buf.len() >= 4096 {
        let sig = &buf[4086..4096];
        if sig == b"SWAPSPACE2" || sig == b"SWAP-SPACE" {
            return Some(FsProbe {
                fs_type: FsType::Swap,
                label: None,
                uuid: None,
            });
        }
    }
    None
}

/// Format 16 raw bytes as a hyphenated UUID string.
fn format_uuid(b: &[u8]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Read a nul-terminated label; `None` if empty.
fn read_cstr(b: &[u8]) -> Option<String> {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let s = String::from_utf8_lossy(&b[..end]).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeroed(n: usize) -> Vec<u8> {
        vec![0u8; n]
    }

    #[test]
    fn detects_ext4_with_label_and_uuid() {
        let mut buf = zeroed(2048);
        buf[1080] = 0x53;
        buf[1081] = 0xEF;
        buf[1128..1144].copy_from_slice(&[
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ]);
        buf[1144..1148].copy_from_slice(b"root");
        let p = probe_fs(&buf).unwrap();
        assert_eq!(p.fs_type, FsType::Ext4);
        assert_eq!(p.label.as_deref(), Some("root"));
        assert_eq!(
            p.uuid.as_deref(),
            Some("01234567-89ab-cdef-fedc-ba9876543210")
        );
    }

    #[test]
    fn detects_xfs() {
        let mut buf = zeroed(512);
        buf[0..4].copy_from_slice(b"XFSB");
        assert_eq!(probe_fs(&buf).unwrap().fs_type, FsType::Xfs);
    }

    #[test]
    fn detects_vfat() {
        let mut buf = zeroed(512);
        buf[510] = 0x55;
        buf[511] = 0xAA;
        buf[82..85].copy_from_slice(b"FAT");
        assert_eq!(probe_fs(&buf).unwrap().fs_type, FsType::Vfat);
    }

    #[test]
    fn detects_swap() {
        let mut buf = zeroed(4096);
        buf[4086..4096].copy_from_slice(b"SWAPSPACE2");
        assert_eq!(probe_fs(&buf).unwrap().fs_type, FsType::Swap);
    }

    #[test]
    fn unknown_is_none() {
        assert!(probe_fs(&zeroed(4096)).is_none());
    }
}
