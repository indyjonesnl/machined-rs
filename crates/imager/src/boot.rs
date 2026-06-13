//! Staging external binaries onto the FAT boot partition (/boot/bin): the
//! containerd static tarball and the runc binary. Distinct from apk extraction
//! (which targets the initramfs rootfs) — these land on disk, not in RAM.

use anyhow::Context;
use std::path::{Component, Path};

/// Guard: every component of an archive entry path must be Normal (no `..`,
/// no absolute, no prefix) — same posture as apk extraction.
fn guard_contained(path: &Path) -> anyhow::Result<()> {
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        anyhow::bail!("boot-tarball entry escapes staging: {}", path.display());
    }
    Ok(())
}

fn set_exec(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod 0755 {}", path.display()))?;
    }
    Ok(())
}

/// Extract `bin/*` files from a single-stream `.tar.gz` into `staging_bin`,
/// flattened (bin/containerd -> staging_bin/containerd), mode 0755. Non-`bin/`
/// entries are ignored. The official containerd static tarball is exactly this
/// shape (bin/containerd, bin/containerd-shim-runc-v2, bin/ctr, …).
///
/// # Errors
/// Fails on I/O errors or an entry whose path escapes containment (`..`/absolute).
pub fn extract_boot_tarball(tgz: &Path, staging_bin: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(tgz).with_context(|| format!("opening {}", tgz.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    std::fs::create_dir_all(staging_bin)
        .with_context(|| format!("create {}", staging_bin.display()))?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        guard_contained(&path)?;
        let mut comps = path.components();
        if comps.next().map(|c| c.as_os_str()) != Some(std::ffi::OsStr::new("bin")) {
            continue; // only bin/* is staged
        }
        let rest: std::path::PathBuf = comps.collect();
        if rest.as_os_str().is_empty() || !entry.header().entry_type().is_file() {
            continue; // the bin/ dir entry itself, or a nested non-file
        }
        let target = staging_bin.join(&rest);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf)?;
        std::fs::write(&target, &buf).with_context(|| format!("write {}", target.display()))?;
        set_exec(&target)?;
    }
    Ok(())
}

/// Copy a single static binary into `staging_bin` under `name`, mode 0755
/// (e.g. runc.amd64 -> staging_bin/runc).
///
/// # Errors
/// Fails on I/O errors.
pub fn copy_boot_binary(src: &Path, staging_bin: &Path, name: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(staging_bin)
        .with_context(|| format!("create {}", staging_bin.display()))?;
    let target = staging_bin.join(name);
    std::fs::copy(src, &target)
        .with_context(|| format!("copy {} -> {}", src.display(), target.display()))?;
    set_exec(&target)?;
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

    fn tar_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            for (name, data) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h.clone(), name, *data).unwrap();
            }
            b.finish().unwrap();
        }
        buf
    }

    #[test]
    fn boot_tarball_stages_bin_entries_executable() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("containerd.tar.gz");
        std::fs::write(
            &tgz,
            gzip(&tar_with(&[
                ("bin/containerd", b"\x7fELF-c"),
                ("bin/containerd-shim-runc-v2", b"\x7fELF-s"),
                ("bin/ctr", b"\x7fELF-x"),
                // non-bin entries are ignored
                ("LICENSE", b"license"),
            ])),
        )
        .unwrap();
        let staging_bin = dir.path().join("staging").join("bin");
        extract_boot_tarball(&tgz, &staging_bin).unwrap();

        assert_eq!(
            std::fs::read(staging_bin.join("containerd")).unwrap(),
            b"\x7fELF-c"
        );
        assert_eq!(
            std::fs::read(staging_bin.join("containerd-shim-runc-v2")).unwrap(),
            b"\x7fELF-s"
        );
        assert_eq!(
            std::fs::read(staging_bin.join("ctr")).unwrap(),
            b"\x7fELF-x"
        );
        assert!(
            !staging_bin.join("LICENSE").exists(),
            "non-bin entries skipped"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(staging_bin.join("containerd"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "must be executable");
        }
    }

    #[test]
    fn boot_tarball_rejects_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("evil.tar.gz");
        // Build a tar carrying a hostile name via raw header bytes (append_data
        // sanitizes), mirroring the apk escape test.
        let mut raw = Vec::new();
        {
            let mut b = tar::Builder::new(&mut raw);
            let mut h = tar::Header::new_gnu();
            let name = b"bin/../../etc/evil";
            h.as_old_mut().name[..name.len()].copy_from_slice(name);
            h.set_size(3);
            h.set_mode(0o755);
            h.set_cksum();
            b.append(&h, &b"bad"[..]).unwrap();
            b.finish().unwrap();
        }
        std::fs::write(&tgz, gzip(&raw)).unwrap();
        let staging_bin = dir.path().join("staging").join("bin");
        let err = extract_boot_tarball(&tgz, &staging_bin).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
    }

    #[test]
    fn boot_binary_copies_with_rename_executable() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("runc.amd64");
        std::fs::write(&src, b"\x7fELF-runc").unwrap();
        let staging_bin = dir.path().join("staging").join("bin");
        copy_boot_binary(&src, &staging_bin, "runc").unwrap();
        assert_eq!(
            std::fs::read(staging_bin.join("runc")).unwrap(),
            b"\x7fELF-runc"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(staging_bin.join("runc"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }
}
