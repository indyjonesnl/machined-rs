//! APK package handling: resolve and extract Alpine `.apk` packages to source
//! files (kernel modules, firmware) consumed by the image and initramfs builders.
//!
//! An `.apk` is concatenated gzip tar streams (signature, control, data); a
//! `MultiGzDecoder` reads them as one. Payload entries are everything not
//! starting with `.` (the metadata entries are `.SIGN.*`, `.PKGINFO`, ...).
//! Gzipped kernel modules (`*.ko.gz`) are decompressed at extraction so the
//! node never needs kernel module-decompression support.

use anyhow::Context;
use flate2::read::{GzDecoder, MultiGzDecoder};
use std::io::Read;
use std::path::Path;

/// Extract the payload of an Alpine `.apk` into `rootfs`.
///
/// Metadata entries (those whose first path component starts with `.`) are
/// skipped. Gzipped kernel modules are decompressed in place, with the `.gz`
/// suffix stripped from the destination.
///
/// # Errors
///
/// Fails if `apk` cannot be opened, if the gzip/tar streams are malformed, if
/// any entry path would escape `rootfs` (absolute paths or `..`/`.`
/// components), if a `.ko.gz` entry cannot be decompressed, or on any
/// extraction I/O error (including the path-traversal refusal that
/// `unpack_in` enforces).
pub fn extract_apk(apk: &Path, rootfs: &Path) -> anyhow::Result<()> {
    let file =
        std::fs::File::open(apk).with_context(|| format!("opening apk {}", apk.display()))?;
    let gz = MultiGzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    // The first entry may be a file at depth 0, so the root must exist up front.
    std::fs::create_dir_all(rootfs)
        .with_context(|| format!("creating rootfs {}", rootfs.display()))?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(first) = path.components().next() else {
            continue; // empty entry path: nothing to extract
        };
        // Containment guard, and it must come BEFORE the metadata skip: a
        // leading `..` also "starts with '.'" and would be silently skipped
        // there, while an absolute path makes `join` below discard rootfs
        // entirely. Only plain Normal components are allowed (no RootDir,
        // no Windows prefix, no `..`/`.`); a malicious archive fails the
        // build loudly instead of escaping or being ignored.
        if !path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            anyhow::bail!("apk entry escapes rootfs: {}", path.display());
        }
        if first.as_os_str().to_string_lossy().starts_with('.') {
            continue; // .SIGN.*, .PKGINFO and friends
        }
        if path.to_string_lossy().ends_with(".ko.gz") {
            // This branch assumes a regular-file entry: hardlinked modules do
            // not occur in Alpine kernel apks (a hardlink entry carries no
            // data and would decompress empty).
            let target = rootfs.join(path.with_extension("")); // strip .gz
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut raw = Vec::new();
            GzDecoder::new(&mut entry)
                .read_to_end(&mut raw)
                .with_context(|| format!("decompressing module {}", path.display()))?;
            std::fs::write(&target, raw)
                .with_context(|| format!("writing module {}", target.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Modules need no exec bit; set 0644 explicitly so the mode
                // is not umask-dependent.
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))?;
            }
        } else {
            // unpack_in refuses '../' path traversal by design.
            entry.unpack_in(rootfs)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn synthetic_apk() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h.clone(), ".PKGINFO", &b"meta"[..])
                .unwrap();
            let mut hd = tar::Header::new_gnu();
            hd.set_size(0);
            hd.set_entry_type(tar::EntryType::Directory);
            hd.set_mode(0o755);
            hd.set_cksum();
            b.append_data(&mut hd, "sbin/", &b""[..]).unwrap();
            let mut hf = tar::Header::new_gnu();
            hf.set_size(5);
            hf.set_mode(0o755);
            hf.set_cksum();
            b.append_data(&mut hf, "sbin/mkfs.ext4", &b"\x7fELF!"[..])
                .unwrap();
            // a gzipped kernel module
            let ko = gzip(b"module-bytes");
            let mut hk = tar::Header::new_gnu();
            hk.set_size(ko.len() as u64);
            hk.set_mode(0o644);
            hk.set_cksum();
            b.append_data(
                &mut hk,
                "lib/modules/6.12.81-0-virt/kernel/fs/ext4/ext4.ko.gz",
                &ko[..],
            )
            .unwrap();
            b.finish().unwrap();
        }
        gzip(&tar_bytes)
    }

    /// e2fsprogs ships `sbin/mkfs.ext4` as a symlink to `mke2fs`; pin that
    /// `unpack_in` reproduces it as a link rather than a copy.
    fn synthetic_apk_with_symlink() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            // a payload regular file so the directory exists
            let mut hf = tar::Header::new_gnu();
            hf.set_size(5);
            hf.set_mode(0o755);
            hf.set_cksum();
            b.append_data(&mut hf, "sbin/mke2fs", &b"\x7fELF!"[..])
                .unwrap();
            // the symlink
            let mut hl = tar::Header::new_gnu();
            hl.set_size(0);
            hl.set_entry_type(tar::EntryType::Symlink);
            hl.set_mode(0o777);
            hl.set_link_name("mke2fs").unwrap();
            hl.set_cksum();
            b.append_data(&mut hl, "sbin/mkfs.ext4", &b""[..]).unwrap();
            b.finish().unwrap();
        }
        gzip(&tar_bytes)
    }

    #[test]
    fn extracts_payload_skips_metadata_decompresses_ko_gz() {
        let dir = tempfile::tempdir().unwrap();
        let apk = dir.path().join("a.apk");
        std::fs::write(&apk, synthetic_apk()).unwrap();
        extract_apk(&apk, dir.path().join("root").as_path()).unwrap();
        let root = dir.path().join("root");
        assert!(!root.join(".PKGINFO").exists(), "metadata must be skipped");
        assert_eq!(
            std::fs::read(root.join("sbin/mkfs.ext4")).unwrap(),
            b"\x7fELF!"
        );
        // .ko.gz arrives decompressed, with the .gz suffix stripped
        assert_eq!(
            std::fs::read(root.join("lib/modules/6.12.81-0-virt/kernel/fs/ext4/ext4.ko")).unwrap(),
            b"module-bytes"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("sbin/mkfs.ext4"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "exec bit must survive");
            let ko_mode =
                std::fs::metadata(root.join("lib/modules/6.12.81-0-virt/kernel/fs/ext4/ext4.ko"))
                    .unwrap()
                    .permissions()
                    .mode();
            assert_eq!(
                ko_mode & 0o777,
                0o644,
                "module mode must not be umask-dependent"
            );
        }
    }

    /// A single-entry apk whose path is written raw into the header so the
    /// builder cannot sanitize it (e.g. `../evil.ko.gz`, `/abs/evil.ko.gz`).
    fn malicious_apk(name: &[u8]) -> Vec<u8> {
        let ko = gzip(b"evil");
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            let mut h = tar::Header::new_gnu();
            h.as_old_mut().name[..name.len()].copy_from_slice(name);
            h.set_size(ko.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, &ko[..]).unwrap();
            b.finish().unwrap();
        }
        gzip(&tar_bytes)
    }

    #[test]
    fn rejects_parent_traversal_in_ko_gz_entry() {
        let dir = tempfile::tempdir().unwrap();
        let apk = dir.path().join("evil.apk");
        std::fs::write(&apk, malicious_apk(b"../evil.ko.gz")).unwrap();
        let root = dir.path().join("root");
        let err = extract_apk(&apk, &root).unwrap_err();
        assert!(err.to_string().contains("escapes rootfs"), "{err}");
        // rootfs is dir/root, so the traversal target would land in dir/.
        assert!(
            !dir.path().join("evil.ko").exists(),
            "must not write outside rootfs"
        );
    }

    #[test]
    fn rejects_absolute_path_in_ko_gz_entry() {
        let dir = tempfile::tempdir().unwrap();
        let apk = dir.path().join("evil.apk");
        std::fs::write(&apk, malicious_apk(b"/abs/evil.ko.gz")).unwrap();
        let root = dir.path().join("root");
        let err = extract_apk(&apk, &root).unwrap_err();
        assert!(err.to_string().contains("escapes rootfs"), "{err}");
        // join() with an absolute path discards rootfs entirely; nothing may
        // appear at the absolute location or inside the rootfs.
        assert!(!std::path::Path::new("/abs/evil.ko").exists());
        assert!(!root.join("abs/evil.ko").exists());
    }

    #[test]
    fn reproduces_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let apk = dir.path().join("e2fsprogs.apk");
        std::fs::write(&apk, synthetic_apk_with_symlink()).unwrap();
        extract_apk(&apk, dir.path().join("root").as_path()).unwrap();
        let root = dir.path().join("root");
        let link = root.join("sbin/mkfs.ext4");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "mkfs.ext4 must be a symlink, not a copied file"
        );
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::path::Path::new("mke2fs")
        );
    }
}
