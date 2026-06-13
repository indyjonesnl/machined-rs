//! Early image-boot steps (pid1 only): load the imager-provided kernel module
//! list, mount the EFI boot partition, seed PKI from it, and prefer the boot
//! partition's machine config. Every step is a silent no-op when its input is
//! absent, so dev runs and tests are untouched.

use std::path::{Path, PathBuf};

use machined_block::BlockBackend;
use machined_platform::{MountSpec, Platform};
use machined_resources::{Resource, ResourceType};
use machined_runtime_core::State;
use tracing::{info, warn};

pub const MODULES_LOAD: &str = "/etc/machined/modules.load";
pub const BOOT_CONFIG: &str = "/boot/config.yaml";
pub const BOOT_PKI: &str = "/boot/pki";

/// Load every module listed (absolute .ko paths, dependency-ordered by the
/// imager). Missing list file = not an image boot = no-op.
pub fn load_modules(platform: &dyn Platform, list: &Path) -> anyhow::Result<()> {
    let Ok(text) = std::fs::read_to_string(list) else {
        return Ok(());
    };
    let (mut loaded, mut failed) = (0u32, 0u32);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match platform.load_module(Path::new(line)) {
            Ok(()) => loaded += 1,
            Err(e) => {
                // best-effort: one bad module must not kill boot.
                warn!("loading module {line}: {e}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        warn!("kernel modules: loaded {loaded}, failed {failed}");
    } else {
        info!("kernel modules: loaded {loaded}");
    }
    Ok(())
}

/// Mount the EFI-labeled GPT partition at /boot (vfat). With multiple EFI
/// candidates the lowest device path wins, deterministically.
pub async fn mount_boot(block: &dyn BlockBackend, platform: &dyn Platform) -> anyhow::Result<()> {
    let vols = match block.list_volumes().await {
        Ok(v) => v,
        Err(e) => {
            info!("boot-partition scan skipped: {e}");
            return Ok(());
        }
    };
    let mut candidates: Vec<_> = vols.iter().filter(|v| v.partition_label == "EFI").collect();
    candidates.sort_by(|a, b| a.device.cmp(&b.device));
    let Some(efi) = candidates.first() else {
        return Ok(());
    };
    if candidates.len() > 1 {
        warn!(
            "{} EFI-labeled partitions found; picking {}",
            candidates.len(),
            efi.device
        );
    }
    if platform.is_mounted("/boot")? {
        return Ok(());
    }
    platform.mount(&MountSpec {
        source: efi.device.clone(),
        target: "/boot".into(),
        fstype: "vfat".into(),
        // ro,nosuid,nodev: machined only reads /boot, and nothing on a FAT
        // partition should ever be a device node or setuid. NOT noexec —
        // M7b will exec containerd from /boot.
        flags: machined_platform::MS_RDONLY
            | machined_platform::MS_NOSUID
            | machined_platform::MS_NODEV,
        data: None,
    })?;
    info!("mounted boot partition {} at /boot", efi.device);
    Ok(())
}

/// Copy a pre-baked PKI from the boot partition to the runtime PKI dir,
/// enforcing 0700/0600 (FAT carries no unix perms). NEVER overwrites an
/// existing PKI dir — same no-silent-re-key posture as PkiError::Partial.
///
/// All-or-nothing: the copy is staged into a temp sibling dir and renamed
/// into place, and a src missing any of the four files is skipped outright.
/// A partially-written dst would poison every future seed (dst exists) AND
/// NodePki::load_or_generate (PkiError::Partial) — API disabled forever on a
/// headless node.
pub fn seed_pki(src: &Path, dst: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::unix::fs::PermissionsExt;

    const FILES: [&str; 4] = ["ca.pem", "ca.key", "server.pem", "server.key"];
    if dst.exists() {
        return Ok(());
    }
    let missing: Vec<&str> = FILES
        .iter()
        .copied()
        .filter(|f| !src.join(f).exists())
        .collect();
    if missing.len() == FILES.len() {
        return Ok(()); // no boot PKI at all = not an image boot = no-op.
    }
    if !missing.is_empty() {
        warn!(
            "boot PKI at {} is partial (missing {missing:?}); not seeding",
            src.display()
        );
        return Ok(());
    }

    // Stage into a same-filesystem sibling, then rename atomically into place.
    let tmp = dst.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp); // stale leftover from a crashed seed
    std::fs::create_dir_all(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))?;
    for f in FILES {
        std::fs::copy(src.join(f), tmp.join(f))
            .with_context(|| format!("copy {} from {}", f, src.display()))?;
        let mode = if f.ends_with(".key") { 0o600 } else { 0o644 };
        std::fs::set_permissions(tmp.join(f), std::fs::Permissions::from_mode(mode))?;
    }
    // fsync each staged file before the rename: ext4's auto_da_alloc heuristic
    // does NOT cover rename-to-a-new-path, so without this a power cut just
    // after the rename could leave a present dst dir shadowing zero-length keys
    // — which load_or_generate would then read as a partial PKI (PkiError) and
    // disable the API forever.
    for f in FILES {
        std::fs::File::open(tmp.join(f))
            .and_then(|fh| fh.sync_all())
            .with_context(|| format!("fsync {f}"))?;
    }
    std::fs::rename(&tmp, dst)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dst.display()))?;
    // Persist the rename itself by syncing the parent directory.
    if let Some(parent) = dst.parent() {
        let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
    }
    info!("seeded PKI from {}", src.display());
    Ok(())
}

/// True iff a `MountStatus` for the STATE volume reports it mounted.
fn state_mounted(state: &State) -> bool {
    state
        .list("block", ResourceType::MountStatus)
        .into_iter()
        .any(|o| matches!(o.spec, Resource::MountStatus(m) if m.volume == "STATE" && m.mounted))
}

/// Block until the STATE volume is mounted (the controller runtime provisions
/// then mounts it), polling at the same 200ms cadence as the supervisor's
/// dep-gate. Returns false on timeout. Callers gate PKI seed/load on this so
/// PKI lands on the persistent ext4 STATE, not the initramfs rootfs it would
/// otherwise be shadowed on.
pub async fn wait_for_state_mount(state: &State, timeout: std::time::Duration) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        if state_mounted(state) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// The boot partition's config wins when it exists.
pub fn pick_config_path(boot: &Path, fallback: &Path) -> PathBuf {
    if boot.exists() {
        boot.to_path_buf()
    } else {
        fallback.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use machined_block::{FakeBlockBackend, FsType, VolumeInfo};
    use machined_platform::FakePlatform;

    fn efi_volume() -> VolumeInfo {
        VolumeInfo {
            device: "/dev/vda1".into(),
            disk: "vda".into(),
            partition_uuid: "u".into(),
            partition_label: "EFI".into(),
            partition_type_guid: "g".into(),
            fs_type: Some(FsType::Vfat),
            fs_label: None,
            fs_uuid: None,
            size_bytes: 1 << 20,
        }
    }

    #[tokio::test]
    async fn loads_modules_in_file_order_and_tolerates_missing_file() {
        let p = Arc::new(FakePlatform::new());
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("modules.load");
        std::fs::write(&list, "/lib/modules/v/a.ko\n/lib/modules/v/b.ko\n").unwrap();
        load_modules(p.as_ref(), &list).unwrap();
        assert_eq!(
            p.modules_loaded(),
            vec!["/lib/modules/v/a.ko", "/lib/modules/v/b.ko"]
        );
        // absent file = silent no-op.
        load_modules(p.as_ref(), &dir.path().join("nope")).unwrap();
        assert_eq!(p.modules_loaded().len(), 2);
    }

    #[tokio::test]
    async fn mounts_first_efi_labeled_partition_at_boot() {
        let backend = FakeBlockBackend::new().with_volume(efi_volume());
        let platform = Arc::new(FakePlatform::new());
        mount_boot(&backend, platform.as_ref()).await.unwrap();
        let rec = platform.recorded.lock().unwrap();
        assert_eq!(rec.mounts.len(), 1);
        assert_eq!(rec.mounts[0].source, "/dev/vda1");
        assert_eq!(rec.mounts[0].target, "/boot");
        assert_eq!(rec.mounts[0].fstype, "vfat");
        assert_eq!(
            rec.mounts[0].flags,
            machined_platform::MS_RDONLY
                | machined_platform::MS_NOSUID
                | machined_platform::MS_NODEV,
            "/boot must be ro,nosuid,nodev"
        );
    }

    #[tokio::test]
    async fn multiple_efi_volumes_pick_lowest_device_deterministically() {
        // Seeded out of order: the pick must sort by device string, not trust
        // the backend's enumeration order.
        let mut second = efi_volume();
        second.device = "/dev/vdb1".into();
        let backend = FakeBlockBackend::new()
            .with_volume(second)
            .with_volume(efi_volume()); // /dev/vda1, listed second
        let platform = Arc::new(FakePlatform::new());
        mount_boot(&backend, platform.as_ref()).await.unwrap();
        let rec = platform.recorded.lock().unwrap();
        assert_eq!(rec.mounts.len(), 1);
        assert_eq!(rec.mounts[0].source, "/dev/vda1");
    }

    #[tokio::test]
    async fn no_efi_volume_means_no_mount() {
        let backend = FakeBlockBackend::new();
        let platform = Arc::new(FakePlatform::new());
        mount_boot(&backend, platform.as_ref()).await.unwrap();
        assert!(platform.recorded.lock().unwrap().mounts.is_empty());
    }

    #[tokio::test]
    async fn already_mounted_boot_is_not_remounted() {
        let backend = FakeBlockBackend::new().with_volume(efi_volume());
        let platform = Arc::new(FakePlatform::new());
        platform
            .mount(&MountSpec {
                source: "/dev/other".into(),
                target: "/boot".into(),
                fstype: "vfat".into(),
                flags: 0,
                data: None,
            })
            .unwrap();
        mount_boot(&backend, platform.as_ref()).await.unwrap();
        let rec = platform.recorded.lock().unwrap();
        // Still exactly the one pre-existing /boot mount.
        assert_eq!(rec.mounts.len(), 1);
        assert_eq!(rec.mounts[0].source, "/dev/other");
    }

    #[test]
    fn seeds_pki_from_boot_when_state_pki_missing() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("boot-pki");
        let dst = tmp.path().join("state-pki");
        std::fs::create_dir_all(&src).unwrap();
        for f in ["ca.pem", "ca.key", "server.pem", "server.key"] {
            std::fs::write(src.join(f), f).unwrap();
        }
        seed_pki(&src, &dst).unwrap();
        for f in ["ca.pem", "ca.key", "server.pem", "server.key"] {
            assert!(dst.join(f).exists(), "{f} copied");
            let mode = std::fs::metadata(dst.join(f)).unwrap().permissions().mode() & 0o777;
            let want = if f.ends_with(".key") { 0o600 } else { 0o644 };
            assert_eq!(mode, want, "{f} mode");
        }
        let dmode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "dst dir 0700");
        assert!(
            !dst.with_extension("tmp").exists(),
            "staging dir renamed away"
        );

        // Each seeded file is non-empty and matches the source (the fsync path must
        // not truncate or drop content).
        for f in ["ca.pem", "ca.key", "server.pem", "server.key"] {
            let got = std::fs::read(dst.join(f)).unwrap();
            assert!(!got.is_empty(), "{f} empty after seed");
            assert_eq!(
                got,
                std::fs::read(src.join(f)).unwrap(),
                "{f} content mismatch"
            );
        }

        // Second call must NEVER overwrite an existing dst.
        std::fs::write(src.join("ca.pem"), "EVIL").unwrap();
        seed_pki(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("ca.pem")).unwrap(),
            "ca.pem",
            "dst ca.pem unchanged"
        );
    }

    #[test]
    fn partial_boot_pki_is_not_seeded_and_creates_no_dst() {
        // A boot PKI missing any of the four files must be ignored entirely:
        // a partially-seeded dst would poison both future seeds (dst exists)
        // and load_or_generate (PkiError::Partial) forever.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("boot-pki");
        let dst = tmp.path().join("state-pki");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("ca.pem"), "ca").unwrap();
        std::fs::write(src.join("ca.key"), "key").unwrap();
        // server.pem / server.key missing.
        seed_pki(&src, &dst).unwrap();
        assert!(
            !dst.exists(),
            "no dst dir may be created from a partial src"
        );
    }

    #[test]
    fn config_path_prefers_boot_config_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let boot = tmp.path().join("boot-config.yaml");
        let fallback = tmp.path().join("etc-config.yaml");
        // boot missing -> fallback.
        assert_eq!(pick_config_path(&boot, &fallback), fallback);
        // boot exists -> boot.
        std::fs::write(&boot, "x").unwrap();
        assert_eq!(pick_config_path(&boot, &fallback), boot);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_true_when_state_mounted() {
        use machined_resources::{MountStatus, Resource, ResourceObject};
        use machined_runtime_core::State;
        let state = State::new();
        state
            .create(ResourceObject::new(
                "block",
                "STATE",
                Resource::MountStatus(MountStatus {
                    volume: "STATE".into(),
                    source: "/dev/vda2".into(),
                    target: "/system/state".into(),
                    fstype: "ext4".into(),
                    mounted: true,
                }),
            ))
            .unwrap();
        assert!(wait_for_state_mount(&state, std::time::Duration::from_secs(60)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_times_out_when_state_absent_or_other_volume() {
        use machined_resources::{MountStatus, Resource, ResourceObject};
        use machined_runtime_core::State;
        let state = State::new();
        // An EPHEMERAL mount must NOT satisfy the STATE wait.
        state
            .create(ResourceObject::new(
                "block",
                "EPHEMERAL",
                Resource::MountStatus(MountStatus {
                    volume: "EPHEMERAL".into(),
                    source: "/dev/vda3".into(),
                    target: "/var".into(),
                    fstype: "ext4".into(),
                    mounted: true,
                }),
            ))
            .unwrap();
        assert!(!wait_for_state_mount(&state, std::time::Duration::from_secs(60)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_unblocks_when_state_appears_mid_wait() {
        use machined_resources::{MountStatus, Resource, ResourceObject};
        use machined_runtime_core::State;
        let state = State::new();
        let s2 = state.clone();
        let waiter = tokio::spawn(async move {
            wait_for_state_mount(&s2, std::time::Duration::from_secs(60)).await
        });
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        state
            .create(ResourceObject::new(
                "block",
                "STATE",
                Resource::MountStatus(MountStatus {
                    volume: "STATE".into(),
                    source: "/dev/vda2".into(),
                    target: "/system/state".into(),
                    fstype: "ext4".into(),
                    mounted: true,
                }),
            ))
            .unwrap();
        assert!(waiter.await.unwrap());
    }
}
