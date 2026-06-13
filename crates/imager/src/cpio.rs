//! Minimal cpio "newc" (070701) archive writer — just what an initramfs needs:
//! directories, regular files, symlinks, and character devices. Format: 6-byte
//! magic + 13 zero-padded 8-hex fields, NUL-terminated name, name and data
//! each padded to 4 bytes; terminated by the TRAILER!!! entry.
//!
//! Reproducible by construction: mtime 0, uid/gid 0, sequential inode numbers.

const S_IFDIR: u32 = 0o040000;
const S_IFCHR: u32 = 0o020000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

pub struct CpioWriter {
    buf: Vec<u8>,
    ino: u32,
}

impl CpioWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            ino: 1,
        }
    }

    /// Append one entry. `rmajor`/`rminor` are the rdev for device nodes (0 otherwise);
    /// `data` is the file contents (link target for symlinks, empty for dirs/devices).
    fn entry(&mut self, name: &str, mode: u32, rmajor: u32, rminor: u32, data: &[u8]) {
        let ino = self.ino;
        self.ino += 1;
        let fields: [u32; 13] = [
            ino,
            mode,
            0, // uid
            0, // gid
            1, // nlink
            0, // mtime (0 = reproducible)
            data.len() as u32,
            0,                     // devmajor
            0,                     // devminor
            rmajor,                // rdevmajor
            rminor,                // rdevminor
            name.len() as u32 + 1, // namesize incl. trailing NUL
            0,                     // check (always 0 for newc)
        ];
        self.buf.extend_from_slice(b"070701");
        for f in fields {
            self.buf.extend_from_slice(format!("{f:08X}").as_bytes());
        }
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(0);
        // The newc spec pads the name from the START of the 110-byte header,
        // and the data from the start of the data, each to a 4-byte multiple.
        // Every entry begins at a 4-aligned global offset (the previous entry's
        // trailing data pad guarantees it; the first entry begins at offset 0),
        // so padding the global buffer to %4 == 0 is equivalent to padding each
        // region from its own start. The `cpio -it` round-trip test is the judge.
        self.pad4();
        self.buf.extend_from_slice(data);
        self.pad4();
    }

    fn pad4(&mut self) {
        while !self.buf.len().is_multiple_of(4) {
            self.buf.push(0);
        }
    }

    pub fn dir(&mut self, name: &str, perm: u32) {
        self.entry(name, S_IFDIR | perm, 0, 0, &[]);
    }

    pub fn file(&mut self, name: &str, perm: u32, data: &[u8]) {
        self.entry(name, S_IFREG | perm, 0, 0, data);
    }

    pub fn symlink(&mut self, name: &str, target: &str) {
        // Symlink permission bits are conventionally 0777; the target path is
        // stored as the entry's data.
        self.entry(name, S_IFLNK | 0o777, 0, 0, target.as_bytes());
    }

    pub fn char_device(&mut self, name: &str, perm: u32, major: u32, minor: u32) {
        self.entry(name, S_IFCHR | perm, major, minor, &[]);
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, 0, 0, &[]);
        self.buf
    }
}

impl Default for CpioWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(archive: &[u8], entry_off: usize, n: usize) -> u32 {
        // header: 6 magic + 13 8-hex fields; field n at 6 + n*8
        let s = std::str::from_utf8(&archive[entry_off + 6 + n * 8..entry_off + 6 + (n + 1) * 8])
            .unwrap();
        u32::from_str_radix(s, 16).unwrap()
    }

    #[test]
    fn writes_parseable_newc_with_device_node_and_trailer() {
        let mut w = CpioWriter::new();
        w.dir("dev", 0o755);
        w.char_device("dev/console", 0o600, 5, 1);
        w.file("init", 0o755, b"#!ELF");
        let bytes = w.finish();
        assert_eq!(&bytes[0..6], b"070701", "newc magic");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("dev/console"));
        assert!(text.contains("TRAILER!!!"));
        assert_eq!(bytes.len() % 4, 0, "archive padded to 4");
        // first entry is the dir: mode field (index 1) = S_IFDIR | 0755
        assert_eq!(field(&bytes, 0, 1), 0o040755);
    }

    #[test]
    fn file_data_is_embedded_and_padded() {
        let mut w = CpioWriter::new();
        w.file("a", 0o644, b"xyz");
        let bytes = w.finish();
        let pos = bytes.windows(3).position(|w| w == b"xyz").unwrap();
        assert_eq!(pos % 4, 0, "data 4-aligned");
    }

    #[test]
    fn symlink_entry_has_symlink_mode_and_target_data() {
        let mut w = CpioWriter::new();
        w.symlink("sbin/mkfs.ext4", "mke2fs");
        let bytes = w.finish();
        // First entry is the symlink: mode field (index 1) = S_IFLNK | 0777.
        assert_eq!(field(&bytes, 0, 1), 0o120777);
        // Its data is the link target bytes.
        let pos = bytes
            .windows(b"mke2fs".len())
            .position(|win| win == b"mke2fs")
            .unwrap();
        assert_eq!(pos % 4, 0, "target data 4-aligned");
    }

    #[test]
    fn round_trips_through_system_cpio() {
        use std::io::Write;
        use std::process::Command;

        // CI's ubuntu has cpio; some dev machines may not. Skip if absent.
        if Command::new("which")
            .arg("cpio")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("skipping: `cpio` not available on this machine");
            return;
        }

        let mut w = CpioWriter::new();
        w.dir("dev", 0o755);
        w.char_device("dev/console", 0o600, 5, 1);
        w.file("init", 0o755, b"#!ELF");
        w.symlink("sbin/mkfs.ext4", "mke2fs");
        let bytes = w.finish();

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&bytes).unwrap();
        tmp.flush().unwrap();

        let out = Command::new("cpio")
            .arg("-it")
            .stdin(std::fs::File::open(tmp.path()).unwrap())
            .output()
            .expect("run cpio -it");
        assert!(
            out.status.success(),
            "cpio rejected the archive: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let listing = String::from_utf8_lossy(&out.stdout);
        for name in ["dev", "dev/console", "init", "sbin/mkfs.ext4"] {
            assert!(
                listing.lines().any(|l| l == name),
                "listing missing {name}:\n{listing}"
            );
        }
    }
}
