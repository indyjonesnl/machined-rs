//! OS upgrade preparation: download an image bundle, verify its sha256, extract
//! the kernel+initramfs, and load them into the kexec buffer — all BEFORE the
//! daemon commits to shutting down, so a failed upgrade leaves the node running.

use std::path::{Path, PathBuf};

use crate::bootloader::BootloaderBackend;
use machined_platform::Platform;
use machined_resources::{Resource, ResourceObject, ResourceType, UpgradePhase, UpgradeStatus};
use machined_runtime_core::{reconcile_owned, State};
use sha2::{Digest, Sha256};
use tracing::{error, info};

const NS: &str = "runtime";
const OWNER: &str = "upgrade";
const STAGE_DIR: &str = "/var/machined-upgrade";

fn publish(state: &State, phase: UpgradePhase, message: &str) {
    let obj = ResourceObject::new(
        NS,
        "upgrade",
        Resource::UpgradeStatus(UpgradeStatus {
            phase,
            message: message.to_string(),
        }),
    );
    let _ = reconcile_owned(state, OWNER, NS, ResourceType::UpgradeStatus, vec![obj]);
}

/// Hex sha256 of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Extract `vmlinuz` + `initramfs.img` from a gzipped-tar bundle into `dir`.
/// Returns their paths. Errors if either entry is missing.
pub fn extract_bundle(tgz: &[u8], dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    let (mut kernel, mut initrd) = (None, None);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned);
        let Some(name) = name else { continue };
        let target = match name.as_str() {
            "vmlinuz" => dir.join("vmlinuz"),
            "initramfs.img" => dir.join("initramfs.img"),
            _ => continue,
        };
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf)?;
        std::fs::write(&target, &buf)?;
        if name == "vmlinuz" {
            kernel = Some(target);
        } else {
            initrd = Some(target);
        }
    }
    match (kernel, initrd) {
        (Some(k), Some(i)) => Ok((k, i)),
        _ => anyhow::bail!("bundle missing vmlinuz and/or initramfs.img"),
    }
}

/// Blocking HTTP GET of `url` into a byte vec (run under spawn_blocking).
fn http_get(url: &str) -> anyhow::Result<Vec<u8>> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf)?;
    Ok(buf)
}

/// Download + verify + extract + kexec_load. On success the new image is in the
/// kexec buffer (caller proceeds to shutdown + reboot_kexec). On ANY failure it
/// publishes UpgradeStatus=Failed and returns Err (caller keeps the node up).
pub async fn prepare(
    state: &State,
    platform: &dyn Platform,
    url: &str,
    sha256: &str,
) -> anyhow::Result<()> {
    publish(state, UpgradePhase::Downloading, url);
    let url_owned = url.to_string();
    let bytes = match tokio::task::spawn_blocking(move || http_get(&url_owned)).await? {
        Ok(b) => b,
        Err(e) => {
            publish(state, UpgradePhase::Failed, &e.to_string());
            return Err(e);
        }
    };

    publish(state, UpgradePhase::Verifying, "");
    let got = sha256_hex(&bytes);
    if !got.eq_ignore_ascii_case(sha256) {
        let msg = format!("sha256 mismatch: got {got}, want {sha256}");
        publish(state, UpgradePhase::Failed, &msg);
        anyhow::bail!(msg);
    }

    let dir = Path::new(STAGE_DIR);
    let (kernel, initrd) = match extract_bundle(&bytes, dir) {
        Ok(v) => v,
        Err(e) => {
            publish(state, UpgradePhase::Failed, &e.to_string());
            return Err(e);
        }
    };

    let cmdline = platform
        .kernel_cmdline()
        .unwrap_or_else(|_| "console=ttyS0".to_string());
    if let Err(e) = platform.kexec_load(&kernel, &initrd, cmdline.trim()) {
        publish(state, UpgradePhase::Failed, &e.to_string());
        error!("kexec_load failed: {e}");
        return Err(anyhow::anyhow!("kexec_load: {e}"));
    }
    info!("upgrade image loaded into kexec buffer");
    publish(state, UpgradePhase::Loaded, "");
    Ok(())
}

/// Verify + extract `bytes` (a bundle.tgz) into `dir`, stage the new
/// kernel+initramfs into the inactive A/B slot via `backend`, and flip the boot
/// pointer. Publishes UpgradeStatus along the way; on ANY failure publishes
/// Failed and returns Err (caller keeps the node up). On success the node is
/// ready to COLD-reboot into the freshly-staged slot.
// Wired into main.rs in Task 6 (alongside SdBootBackend); unused until then and
// would trip `dead_code` under CI's `-D warnings`. Mirrors bootloader.rs.
#[allow(dead_code)]
pub async fn stage_and_commit(
    state: &State,
    backend: &dyn BootloaderBackend,
    dir: &Path,
    bytes: &[u8],
    sha256: &str,
) -> anyhow::Result<()> {
    publish(state, UpgradePhase::Verifying, "");
    let got = sha256_hex(bytes);
    if !got.eq_ignore_ascii_case(sha256) {
        let msg = format!("sha256 mismatch: got {got}, want {sha256}");
        publish(state, UpgradePhase::Failed, &msg);
        anyhow::bail!(msg);
    }
    let (kernel, initrd) = match extract_bundle(bytes, dir) {
        Ok(v) => v,
        Err(e) => {
            publish(state, UpgradePhase::Failed, &e.to_string());
            return Err(e);
        }
    };
    let slot = match backend.stage_inactive(&kernel, &initrd) {
        Ok(s) => s,
        Err(e) => {
            publish(state, UpgradePhase::Failed, &e.to_string());
            return Err(e);
        }
    };
    if let Err(e) = backend.set_active(slot) {
        publish(state, UpgradePhase::Failed, &e.to_string());
        return Err(e);
    }
    info!(
        "upgrade staged to slot {} and committed; cold reboot to apply",
        slot.id()
    );
    publish(state, UpgradePhase::Staged, slot.id());
    Ok(())
}

/// Download the bundle from `url`, then stage+commit it into STAGE_DIR (see
/// [`stage_and_commit`]). On success the node is ready to COLD-reboot into the
/// new slot.
#[allow(dead_code)]
pub async fn prepare_disk(
    state: &State,
    backend: &dyn BootloaderBackend,
    url: &str,
    sha256: &str,
) -> anyhow::Result<()> {
    publish(state, UpgradePhase::Downloading, url);
    let url_owned = url.to_string();
    let bytes = match tokio::task::spawn_blocking(move || http_get(&url_owned)).await? {
        Ok(b) => b,
        Err(e) => {
            publish(state, UpgradePhase::Failed, &e.to_string());
            return Err(e);
        }
    };
    stage_and_commit(state, backend, Path::new(STAGE_DIR), &bytes, sha256).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::Slot;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    #[derive(Default)]
    struct StubBackend {
        staged: std::sync::Mutex<Option<Slot>>,
        active: std::sync::Mutex<Option<Slot>>,
        fail_set_active: bool,
    }
    impl crate::bootloader::BootloaderBackend for StubBackend {
        fn current_slot(&self) -> Slot {
            Slot::A
        }
        fn stage_inactive(&self, _k: &Path, _i: &Path) -> anyhow::Result<Slot> {
            *self.staged.lock().unwrap() = Some(Slot::B);
            Ok(Slot::B)
        }
        fn set_active(&self, s: Slot) -> anyhow::Result<()> {
            if self.fail_set_active {
                anyhow::bail!("inject");
            }
            *self.active.lock().unwrap() = Some(s);
            Ok(())
        }
    }

    fn bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            for (n, d) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(d.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, n, *d).unwrap();
            }
            b.finish().unwrap();
        }
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn sha256_hex_known_vector() {
        // sha256("") = e3b0c442...
        assert_eq!(&sha256_hex(b"")[..8], "e3b0c442");
    }

    #[test]
    fn extract_bundle_pulls_both_files() {
        let tgz = bundle(&[
            ("vmlinuz", b"KERNEL"),
            ("initramfs.img", b"INITRD"),
            ("README", b"x"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let (k, i) = extract_bundle(&tgz, dir.path()).unwrap();
        assert_eq!(std::fs::read(&k).unwrap(), b"KERNEL");
        assert_eq!(std::fs::read(&i).unwrap(), b"INITRD");
    }

    #[test]
    fn extract_bundle_missing_kernel_errors() {
        let tgz = bundle(&[("initramfs.img", b"INITRD")]);
        let dir = tempfile::tempdir().unwrap();
        assert!(extract_bundle(&tgz, dir.path()).is_err());
    }

    #[test]
    fn publish_writes_upgrade_status() {
        use machined_resources::Key;
        let state = State::new();
        super::publish(&state, UpgradePhase::Failed, "sha mismatch");
        let got = state
            .get(&Key::new(NS, ResourceType::UpgradeStatus, "upgrade"))
            .unwrap();
        assert!(
            matches!(got.spec, Resource::UpgradeStatus(u) if u.phase == UpgradePhase::Failed && u.message == "sha mismatch")
        );
    }

    #[tokio::test]
    async fn stage_and_commit_stages_inactive_then_flips_and_publishes_staged() {
        use machined_resources::Key;
        let state = State::new();
        let dir = tempfile::tempdir().unwrap();
        let tgz = bundle(&[("vmlinuz", b"K"), ("initramfs.img", b"I")]);
        let sha = sha256_hex(&tgz);
        let be = StubBackend::default();
        super::stage_and_commit(&state, &be, dir.path(), &tgz, &sha)
            .await
            .unwrap();
        assert_eq!(*be.staged.lock().unwrap(), Some(Slot::B));
        assert_eq!(*be.active.lock().unwrap(), Some(Slot::B));
        let got = state
            .get(&Key::new(NS, ResourceType::UpgradeStatus, "upgrade"))
            .unwrap();
        assert!(
            matches!(got.spec, Resource::UpgradeStatus(u) if u.phase == UpgradePhase::Staged && u.message == "b")
        );
    }

    #[tokio::test]
    async fn stage_and_commit_sha_mismatch_fails_and_publishes_failed() {
        let state = State::new();
        let dir = tempfile::tempdir().unwrap();
        let tgz = bundle(&[("vmlinuz", b"K"), ("initramfs.img", b"I")]);
        let be = StubBackend::default();
        let err = super::stage_and_commit(&state, &be, dir.path(), &tgz, "deadbeef").await;
        assert!(err.is_err());
        assert_eq!(
            *be.staged.lock().unwrap(),
            None,
            "must not stage on bad sha"
        );
    }

    // stage_inactive succeeds but set_active fails: the inactive slot is written
    // yet the boot pointer is NOT flipped, so a cold reboot still lands on the
    // old slot. stage_and_commit must surface the error and publish Failed.
    #[tokio::test]
    async fn stage_and_commit_set_active_failure_aborts_and_publishes_failed() {
        use machined_resources::Key;
        let state = State::new();
        let dir = tempfile::tempdir().unwrap();
        let tgz = bundle(&[("vmlinuz", b"K"), ("initramfs.img", b"I")]);
        let sha = sha256_hex(&tgz);
        let be = StubBackend {
            fail_set_active: true,
            ..Default::default()
        };
        let err = super::stage_and_commit(&state, &be, dir.path(), &tgz, &sha).await;
        assert!(err.is_err());
        // inactive slot was written, but the pointer never flipped.
        assert_eq!(*be.staged.lock().unwrap(), Some(Slot::B));
        assert_eq!(*be.active.lock().unwrap(), None);
        let got = state
            .get(&Key::new(NS, ResourceType::UpgradeStatus, "upgrade"))
            .unwrap();
        assert!(matches!(got.spec, Resource::UpgradeStatus(u) if u.phase == UpgradePhase::Failed));
    }
}
