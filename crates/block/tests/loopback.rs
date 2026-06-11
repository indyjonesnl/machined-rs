//! Privileged integration test: write a GPT into a sparse file, attach it as a
//! loop device, and discover it through the real SysfsBlock. Ignored by default;
//! run with: sudo -E cargo test -p machined-block --test loopback -- --ignored
//! Requires root (losetup) + CAP_SYS_ADMIN.

#![cfg(target_os = "linux")]

use std::process::Command;

use machined_block::{BlockBackend, SysfsBlock};

#[tokio::test]
#[ignore = "requires root + losetup"]
async fn discovers_loopback_disk_partitions() {
    let img = std::env::temp_dir().join("mnd-loop.img");
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(64 * 1024 * 1024).unwrap();
    }
    // Write a GPT with one partition using the gpt crate.
    let mut g = gpt::GptConfig::new()
        .writable(true)
        .initialized(false)
        .open(&img)
        .unwrap();
    g.update_partitions(std::collections::BTreeMap::new())
        .unwrap();
    g.add_partition(
        "STATE",
        8 * 1024 * 1024,
        gpt::partition_types::LINUX_FS,
        0,
        None,
    )
    .unwrap();
    g.write().unwrap();

    // Attach as a loop device with partition scanning (-P).
    let out = Command::new("losetup")
        .args(["-fP", "--show", img.to_str().unwrap()])
        .output()
        .expect("losetup");
    assert!(out.status.success(), "losetup failed");
    let loopdev = String::from_utf8(out.stdout).unwrap().trim().to_string(); // e.g. /dev/loop3
    let loopname = loopdev.trim_start_matches("/dev/").to_string();

    let be = SysfsBlock::new();
    let disks = be.list_disks().await.unwrap();
    assert!(
        disks.iter().any(|d| d.name == loopname),
        "loop disk discovered"
    );

    let vols = be.list_volumes().await.unwrap();
    let found = vols
        .iter()
        .any(|v| v.disk == loopname && v.partition_label == "STATE");

    // Detach before asserting so we always clean up.
    let _ = Command::new("losetup").args(["-d", &loopdev]).status();
    std::fs::remove_file(&img).ok();

    assert!(found, "STATE partition discovered on loop device");
}

#[tokio::test]
#[ignore = "requires root + losetup + mkfs"]
async fn provisions_loopback_disk() {
    use machined_block::{BlockProvisioner, FsType, PartType, PartitionPlan, SysfsBlock};

    let img = std::env::temp_dir().join("mnd-prov.img");
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(256 * 1024 * 1024).unwrap();
    }
    let out = std::process::Command::new("losetup")
        .args(["-fP", "--show", img.to_str().unwrap()])
        .output()
        .expect("losetup");
    assert!(out.status.success(), "losetup failed");
    let loopdev = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let be = SysfsBlock::new();
    let plan = vec![
        PartitionPlan {
            label: "EFI".into(),
            part_type: PartType::EfiSystem,
            fs: FsType::Vfat,
            size_bytes: 64 * 1024 * 1024,
        },
        PartitionPlan {
            label: "STATE".into(),
            part_type: PartType::LinuxFilesystem,
            fs: FsType::Ext4,
            size_bytes: 0,
        },
    ];
    be.wipe(&loopdev).await.unwrap();
    let devs = be.create_partitions(&loopdev, &plan).await.unwrap();
    be.format(&devs[0], FsType::Vfat, "EFI").await.unwrap();
    be.format(&devs[1], FsType::Ext4, "STATE").await.unwrap();

    let vols = be.list_volumes().await.unwrap();
    let loopname = loopdev.trim_start_matches("/dev/").to_string();
    let found_state = vols.iter().any(|v| {
        v.disk == loopname && v.partition_label == "STATE" && v.fs_type == Some(FsType::Ext4)
    });

    let _ = std::process::Command::new("losetup")
        .args(["-d", &loopdev])
        .status();
    std::fs::remove_file(&img).ok();

    assert!(
        found_state,
        "STATE ext4 partition discovered after provisioning"
    );
}
