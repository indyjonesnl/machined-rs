//! Assembles the initramfs: the extracted apk rootfs + machined as /init +
//! /dev/console + the ordered module list machined loads at early boot.

use crate::cpio::CpioWriter;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::Write;
use std::path::Path;

/// Build a gzip'd newc cpio. `module_paths` are .ko paths relative to
/// /lib/modules/<kver>, already dependency-ordered (Task 4).
///
/// # Errors
/// Returns an error if the machined binary or any rootfs file cannot be read,
/// or if the rootfs contains a top-level `init` entry (which would collide with
/// machined-as-/init).
pub fn build_initramfs(
    rootfs: &Path,
    machined: &Path,
    module_paths: &[String],
    kver: &str,
    image_id: &str,
) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;

    // machined is installed as /init; a rootfs file named `init` would be a real
    // conflict (not the harmless duplicate-dir case below). Bail before walking.
    if rootfs.join("init").symlink_metadata().is_ok() {
        anyhow::bail!("rootfs contains a top-level `init`, which collides with machined-as-/init");
    }

    let mut w = CpioWriter::new();
    // Pre-create the base tree. A rootfs subdir named e.g. `etc` will emit a
    // duplicate dir entry during add_tree — harmless: the kernel ignores the
    // resulting mkdir EEXIST.
    for d in [
        "dev",
        "proc",
        "sys",
        "run",
        "tmp",
        "etc",
        "etc/machined",
        "boot",
        "system",
        "system/state",
        "var",
    ] {
        w.dir(d, 0o755);
    }
    w.char_device("dev/console", 0o600, 5, 1);
    w.file(
        "init",
        0o755,
        &std::fs::read(machined)
            .with_context(|| format!("reading machined binary {}", machined.display()))?,
    );
    let modules_load: String = module_paths
        .iter()
        .map(|p| format!("/lib/modules/{kver}/{p}\n"))
        .collect();
    w.file("etc/machined/modules.load", 0o644, modules_load.as_bytes());
    w.file("etc/machined/image-id", 0o644, image_id.as_bytes());
    add_tree(&mut w, rootfs, rootfs)?;

    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&w.finish())?;
    Ok(gz.finish()?)
}

fn add_tree(w: &mut CpioWriter, root: &Path, dir: &Path) -> anyhow::Result<()> {
    use anyhow::Context;

    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading dir {}", dir.display()))?
        .collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name()); // deterministic archives
    for entry in entries {
        let path = entry.path();
        let rel = path.strip_prefix(root)?.to_string_lossy().to_string();
        let meta = path.symlink_metadata()?; // does NOT follow symlinks
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            w.symlink(&rel, &target.to_string_lossy());
        } else if meta.is_dir() {
            w.dir(&rel, 0o755);
            add_tree(w, root, &path)?;
        } else {
            use std::os::unix::fs::PermissionsExt;
            let data = std::fs::read(&path)
                .with_context(|| format!("reading rootfs file {}", path.display()))?;
            w.file(&rel, meta.permissions().mode() & 0o7777, &data);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    #[test]
    fn builds_gzip_cpio_with_init_console_and_modules_load() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("sbin")).unwrap();
        std::fs::write(rootfs.join("sbin/mkfs.ext4"), b"elf").unwrap();
        let machined = dir.path().join("machined");
        std::fs::write(&machined, b"machined-elf").unwrap();

        let bytes = build_initramfs(
            &rootfs,
            &machined,
            &[
                "kernel/fs/ext4/ext4.ko".into(),
                "kernel/drivers/block/virtio_blk.ko".into(),
            ],
            "6.12.81-0-virt",
            "test-image",
        )
        .unwrap();

        let mut raw = Vec::new();
        GzDecoder::new(&bytes[..]).read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("init"));
        assert!(text.contains("dev/console"));
        assert!(text.contains("sbin/mkfs.ext4"));
        assert!(text.contains("etc/machined/modules.load"));
        assert!(text.contains("etc/machined/image-id"));
        // modules.load content: absolute, ordered paths
        let want = "/lib/modules/6.12.81-0-virt/kernel/fs/ext4/ext4.ko\n/lib/modules/6.12.81-0-virt/kernel/drivers/block/virtio_blk.ko\n";
        assert!(
            text.contains(want),
            "ordered absolute module paths embedded"
        );
        assert!(text.contains("TRAILER!!!"));
    }

    #[test]
    fn preserves_rootfs_symlinks_via_cpio_round_trip() {
        use std::io::Write;
        use std::os::unix::fs::symlink;
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("sbin")).unwrap();
        std::fs::write(rootfs.join("sbin/mke2fs"), b"elf").unwrap();
        symlink("mke2fs", rootfs.join("sbin/mkfs.ext4")).unwrap();
        let machined = dir.path().join("machined");
        std::fs::write(&machined, b"machined-elf").unwrap();

        let bytes =
            build_initramfs(&rootfs, &machined, &[], "6.12.81-0-virt", "test-image").unwrap();
        let mut raw = Vec::new();
        GzDecoder::new(&bytes[..]).read_to_end(&mut raw).unwrap();

        // Byte-level guarantee independent of cpio availability: the entry for
        // sbin/mkfs.ext4 must carry the symlink mode (S_IFLNK | 0777).
        let name = b"sbin/mkfs.ext4\0";
        let name_pos = raw
            .windows(name.len())
            .position(|win| win == name)
            .expect("mkfs.ext4 entry present");
        // newc header: 6 magic + 13*8 hex; mode is field index 1, name follows.
        let header_start = name_pos - 110;
        assert_eq!(
            &raw[header_start..header_start + 6],
            b"070701",
            "newc magic"
        );
        let mode_hex = std::str::from_utf8(&raw[header_start + 14..header_start + 22]).unwrap();
        assert_eq!(
            u32::from_str_radix(mode_hex, 16).unwrap(),
            0o120777,
            "sbin/mkfs.ext4 is a symlink entry"
        );

        // CI's ubuntu has cpio; some dev machines may not. Skip the listing
        // round-trip if absent.
        if Command::new("which")
            .arg("cpio")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("skipping cpio round-trip: `cpio` not available on this machine");
            return;
        }

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&raw).unwrap();
        tmp.flush().unwrap();
        let out = Command::new("cpio")
            .arg("-itv")
            .stdin(std::fs::File::open(tmp.path()).unwrap())
            .output()
            .expect("run cpio -itv");
        assert!(
            out.status.success(),
            "cpio rejected the archive: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(
            listing
                .lines()
                .any(|l| l.starts_with("lrwxrwxrwx") && l.contains("sbin/mkfs.ext4 -> mke2fs")),
            "expected symlink listing for sbin/mkfs.ext4 -> mke2fs:\n{listing}"
        );
    }

    #[test]
    fn output_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("sbin")).unwrap();
        std::fs::create_dir_all(rootfs.join("usr/lib")).unwrap();
        std::fs::write(rootfs.join("sbin/mkfs.ext4"), b"elf").unwrap();
        std::fs::write(rootfs.join("usr/lib/libc.so"), b"libc").unwrap();
        let machined = dir.path().join("machined");
        std::fs::write(&machined, b"machined-elf").unwrap();

        let modules = ["kernel/fs/ext4/ext4.ko".into()];
        let a =
            build_initramfs(&rootfs, &machined, &modules, "6.12.81-0-virt", "test-image").unwrap();
        let b =
            build_initramfs(&rootfs, &machined, &modules, "6.12.81-0-virt", "test-image").unwrap();
        assert_eq!(a, b, "identical inputs must yield byte-identical archives");
    }

    #[test]
    fn rejects_rootfs_with_top_level_init() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(rootfs.join("init"), b"impostor").unwrap();
        let machined = dir.path().join("machined");
        std::fs::write(&machined, b"machined-elf").unwrap();

        let err =
            build_initramfs(&rootfs, &machined, &[], "6.12.81-0-virt", "test-image").unwrap_err();
        assert!(
            err.to_string().contains("init"),
            "error should mention the init collision: {err}"
        );
    }
}
