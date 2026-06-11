//! Privileged integration test: mount a real loop-backed ext4 filesystem via
//! LinuxPlatform and confirm is_mounted. Ignored by default; run with:
//!   sudo -E cargo test -p machined-platform --test loopback_mount -- --ignored
//! Requires root (losetup + mkfs.ext4 + mount).

#![cfg(target_os = "linux")]

use std::process::Command;

use machined_platform::{LinuxPlatform, MountSpec, Platform};

#[test]
#[ignore = "requires root (losetup + mkfs + mount)"]
fn mounts_loopback_ext4() {
    let img = std::env::temp_dir().join("mnd-mount.img");
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(32 * 1024 * 1024).unwrap();
    }
    let out = Command::new("losetup")
        .args(["-f", "--show", img.to_str().unwrap()])
        .output()
        .expect("losetup");
    assert!(out.status.success(), "losetup failed");
    let loopdev = String::from_utf8(out.stdout).unwrap().trim().to_string();

    assert!(Command::new("mkfs.ext4")
        .args(["-F", &loopdev])
        .status()
        .unwrap()
        .success());

    let target = std::env::temp_dir().join("mnd-mnt");
    std::fs::create_dir_all(&target).unwrap();
    let p = LinuxPlatform::new();
    p.mount(&MountSpec {
        source: loopdev.clone(),
        target: target.to_string_lossy().to_string(),
        fstype: "ext4".into(),
        flags: 0,
        data: None,
    })
    .unwrap();

    let mounted = p.is_mounted(&target.to_string_lossy()).unwrap();

    // Cleanup before asserting.
    let _ = Command::new("umount").arg(&target).status();
    let _ = Command::new("losetup").args(["-d", &loopdev]).status();
    std::fs::remove_file(&img).ok();
    std::fs::remove_dir_all(&target).ok();

    assert!(mounted, "loopback ext4 should be mounted");
}
