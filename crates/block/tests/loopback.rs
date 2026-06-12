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

#[tokio::test]
#[ignore = "requires root + losetup + mkfs + mount"]
async fn completes_layout_on_image_disk() {
    // Simulate a freshly-flashed image AS BOOTED: write GPT + EFI only, format
    // EFI, MOUNT it (pid1 mounts /boot before the runtime starts — this makes
    // BLKRRPART return EBUSY and forces the BLKPG_ADD_PARTITION fallback),
    // then APPEND STATE+EPHEMERAL via add_partitions and verify the new nodes
    // appear, format succeeds, and EFI survived untouched.
    use machined_block::{BlockProvisioner, FsType, PartType, PartitionPlan, SysfsBlock};

    let img = std::env::temp_dir().join("mnd-complete.img");
    {
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(256 * 1024 * 1024).unwrap();
    }
    // The imager's output: GPT + a single EFI partition.
    let mut g = gpt::GptConfig::new()
        .writable(true)
        .initialized(false)
        .open(&img)
        .unwrap();
    g.update_partitions(std::collections::BTreeMap::new())
        .unwrap();
    g.add_partition("EFI", 64 * 1024 * 1024, gpt::partition_types::EFI, 0, None)
        .unwrap();
    g.write().unwrap();

    let out = std::process::Command::new("losetup")
        .args(["-fP", "--show", img.to_str().unwrap()])
        .output()
        .expect("losetup");
    assert!(out.status.success(), "losetup failed");
    let loopdev = String::from_utf8(out.stdout).unwrap().trim().to_string();

    // Format + MOUNT EFI before appending — the production scenario. With a
    // mounted partition on the disk, BLKRRPART fails EBUSY and add_partitions
    // must take the BLKPG per-partition path.
    let efi_dev = format!("{loopdev}p1");
    let st = std::process::Command::new("mkfs.vfat")
        .args(["-n", "EFI", &efi_dev])
        .status()
        .expect("mkfs.vfat");
    assert!(st.success(), "mkfs.vfat EFI failed");
    let mnt = std::env::temp_dir().join("mnd-complete-mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let st = std::process::Command::new("mount")
        .args([efi_dev.as_str(), mnt.to_str().unwrap()])
        .status()
        .expect("mount");
    assert!(st.success(), "mount EFI failed");

    let be = SysfsBlock::new();
    let layout = vec![
        PartitionPlan {
            label: "STATE".into(),
            part_type: PartType::LinuxFilesystem,
            fs: FsType::Ext4,
            size_bytes: 32 * 1024 * 1024,
        },
        PartitionPlan {
            label: "EPHEMERAL".into(),
            part_type: PartType::LinuxFilesystem,
            fs: FsType::Ext4,
            size_bytes: 0,
        },
    ];
    // APPEND only — no wipe, no create_partitions. EFI is mounted, so this
    // exercises the BLKPG fallback end-to-end.
    let devs = be.add_partitions(&loopdev, &layout).await.unwrap();
    // EFI is partition 1; the appended partitions are 2 and 3.
    assert_eq!(devs, vec![format!("{loopdev}p2"), format!("{loopdev}p3")]);
    // add_partitions verified node existence itself, but pin it here too: the
    // whole point of the fallback is that these nodes exist despite EBUSY.
    assert!(
        std::path::Path::new(&devs[0]).exists(),
        "{} missing",
        devs[0]
    );
    assert!(
        std::path::Path::new(&devs[1]).exists(),
        "{} missing",
        devs[1]
    );
    be.format(&devs[0], FsType::Ext4, "STATE").await.unwrap();
    be.format(&devs[1], FsType::Ext4, "EPHEMERAL")
        .await
        .unwrap();

    // RE-ENTRY GUARD: a second append of the same labels must refuse loudly.
    let dup = be.add_partitions(&loopdev, &layout).await;
    assert!(dup.is_err(), "duplicate-label append must refuse");

    let vols = be.list_volumes().await.unwrap();
    let loopname = loopdev.trim_start_matches("/dev/").to_string();
    let efi_survived = vols
        .iter()
        .any(|v| v.disk == loopname && v.partition_label == "EFI");
    let state_added = vols.iter().any(|v| {
        v.disk == loopname && v.partition_label == "STATE" && v.fs_type == Some(FsType::Ext4)
    });
    let ephemeral_added = vols.iter().any(|v| {
        v.disk == loopname && v.partition_label == "EPHEMERAL" && v.fs_type == Some(FsType::Ext4)
    });

    // Unmount EFI BEFORE detaching the loop device, then clean up.
    let _ = std::process::Command::new("umount")
        .arg(mnt.to_str().unwrap())
        .status();
    std::fs::remove_dir(&mnt).ok();
    let _ = std::process::Command::new("losetup")
        .args(["-d", &loopdev])
        .status();
    std::fs::remove_file(&img).ok();

    assert!(
        efi_survived,
        "EFI partition untouched after completing layout"
    );
    assert!(state_added, "STATE ext4 appended");
    assert!(ephemeral_added, "EPHEMERAL ext4 appended");
}
