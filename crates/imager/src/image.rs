//! Userspace disk-image writer: protective MBR + GPT with a single 512 MiB
//! FAT32 partition labeled EFI, populated from a staging directory. STATE and
//! EPHEMERAL are deliberately absent — machined completes the layout on first
//! boot, sized to the real disk (`CompleteLayout`, provision.rs). No root, no
//! loop devices: the GPT is written into a plain file and the FAT region is
//! reached through a bounds-checked `fscommon::StreamSlice`.

use anyhow::Context as _;
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::Path;

/// Logical block size of the image (bytes per sector).
const LB: u64 = 512;
/// EFI partition size — matches `fixed_layout()` so a later full re-provision
/// reproduces the same geometry (crates/controllers/src/block/provision.rs).
const EFI_SIZE: u64 = 512 * 1024 * 1024;
/// Slack for the protective MBR, both GPT headers and partition arrays.
const GPT_OVERHEAD: u64 = 4 * 1024 * 1024;

/// Write a bootable, EFI-only disk image to `img`.
///
/// Creates a sparse file of `size` bytes containing a protective MBR, a GPT
/// with one 512 MiB partition (label `EFI`, type EFI System), formats that
/// partition as FAT32 and copies the `staging` tree into it. The kernel
/// requires the protective MBR to recognize the GPT.
///
/// # Errors
/// Returns an error if `size` is too small to hold the EFI partition plus GPT
/// overhead, if the image cannot be created/written, or if the staging tree
/// cannot be read or copied into the FAT filesystem.
pub fn write_image(
    img: &Path,
    size: u64,
    staging: &Path,
    scheme: crate::arch::PartScheme,
) -> anyhow::Result<()> {
    match scheme {
        crate::arch::PartScheme::Gpt => write_image_gpt(img, size, staging),
        crate::arch::PartScheme::Mbr => write_image_mbr(img, size, staging),
    }
}

/// GPT + protective MBR with a single 512 MiB EFI-labeled FAT32 partition.
///
/// # Errors
/// See [`write_image`].
fn write_image_gpt(img: &Path, size: u64, staging: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        size >= EFI_SIZE + GPT_OVERHEAD,
        "image size {size} too small (need at least {} bytes)",
        EFI_SIZE + GPT_OVERHEAD
    );

    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(img)
        .with_context(|| format!("create image {}", img.display()))?;
    file.set_len(size)
        .with_context(|| format!("size image to {size} bytes"))?;

    // Protective MBR first: gpt's writer expects LBA0 occupied, and the kernel
    // refuses to read a GPT without one.
    let last_lba = u32::try_from((size / LB) - 1).unwrap_or(0xFFFF_FFFF);
    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(last_lba);
    mbr.overwrite_lba0(&mut file)
        .context("write protective MBR")?;
    drop(file);

    // Fresh GPT with the single EFI partition. Mirrors block::sysfs::
    // create_partitions: add_partition takes the size in BYTES.
    let mut gdisk = gpt::GptConfig::new()
        .writable(true)
        .initialized(false)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .open(img)
        .context("open image for GPT write")?;
    gdisk
        .update_partitions(BTreeMap::new())
        .context("initialize empty GPT")?;
    gdisk
        .add_partition("EFI", EFI_SIZE, gpt::partition_types::EFI, 0, None)
        .context("add EFI partition")?;
    // gdisk.write() consumes the handle, so capture geometry first.
    let parts = gdisk.partitions().clone();
    let p = parts
        .values()
        .next()
        .context("EFI partition missing after add")?;
    let (start, end) = (p.first_lba * LB, (p.last_lba + 1) * LB);
    gdisk.write().context("write GPT")?;

    // Format + populate the FAT region through a bounds-checked slice. The slice
    // spans the partition's LBAs; StreamSlice's end offset is exclusive.
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(img)
        .context("reopen image for FAT write")?;
    let mut slice = fscommon::StreamSlice::new(file, start, end).context("slice FAT region")?;
    fatfs::format_volume(
        &mut slice,
        fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat32)
            .volume_label(*b"EFI        "),
    )
    .context("format EFI partition as FAT32")?;
    let fs =
        fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).context("mount FAT filesystem")?;
    copy_tree(&fs.root_dir(), staging).context("populate FAT filesystem")?;
    // Dropping `fs` flushes and unmounts; let it fall out of scope here.
    Ok(())
}

/// Classic MBR: one FAT32 primary partition (type 0x0C, bootable) starting at
/// LBA 2048, spanning the rest of the disk. Pi 3 firmware reads the MBR to find
/// the boot FAT (it does not parse GPT). No second partition — machined boots
/// entirely from the initramfs + this FAT.
///
/// # Errors
/// Fails on I/O errors or if the image is too small.
fn write_image_mbr(img: &Path, size: u64, staging: &Path) -> anyhow::Result<()> {
    use std::io::{Seek as _, SeekFrom, Write as _};
    const START_LBA: u64 = 2048; // 1 MiB alignment
    anyhow::ensure!(size > (START_LBA + 2048) * LB, "image too small");
    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(img)
        .with_context(|| format!("create {}", img.display()))?;
    file.set_len(size)?;

    let total_sectors = size / LB;
    let part_sectors = u32::try_from(total_sectors - START_LBA).unwrap_or(u32::MAX);
    let lba_start = u32::try_from(START_LBA).unwrap_or(u32::MAX);

    let mut mbr = [0u8; 512];
    let e = 446;
    mbr[e] = 0x80; // bootable
    mbr[e + 1] = 0xFE;
    mbr[e + 2] = 0xFF;
    mbr[e + 3] = 0xFF; // CHS start: LBA sentinel
    mbr[e + 4] = 0x0C; // type: FAT32 LBA
    mbr[e + 5] = 0xFE;
    mbr[e + 6] = 0xFF;
    mbr[e + 7] = 0xFF; // CHS end: LBA sentinel
    mbr[e + 8..e + 12].copy_from_slice(&lba_start.to_le_bytes());
    mbr[e + 12..e + 16].copy_from_slice(&part_sectors.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&mbr)?;
    drop(file);

    let (start, end) = (START_LBA * LB, (START_LBA + u64::from(part_sectors)) * LB);
    let file = std::fs::File::options().read(true).write(true).open(img)?;
    let mut slice = fscommon::StreamSlice::new(file, start, end)?;
    fatfs::format_volume(
        &mut slice,
        fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat32)
            .volume_label(*b"BOOT       "),
    )?;
    let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new())?;
    copy_tree(&fs.root_dir(), staging)?;
    Ok(())
}

/// Recursively copy a directory tree from `src` into a FAT directory.
///
/// Symlinks in `staging` are FOLLOWED: `std::fs::metadata`/`File::open` follow
/// by default, and staging content comes from our own build step, so following
/// is the useful semantic (FAT has no symlink concept of its own).
///
/// # Errors
/// Returns an error if the source directory cannot be read or any file/dir
/// cannot be created or written in the FAT filesystem.
fn copy_tree(
    dir: &fatfs::Dir<'_, fscommon::StreamSlice<std::fs::File>>,
    src: &Path,
) -> anyhow::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(src)
        .with_context(|| format!("read staging dir {}", src.display()))?
        .collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        // metadata() follows symlinks: see the doc comment above.
        if entry.metadata()?.is_dir() {
            let sub = dir
                .create_dir(&name)
                .with_context(|| format!("create dir {name}"))?;
            copy_tree(&sub, &entry.path())?;
        } else {
            let mut f = dir
                .create_file(&name)
                .with_context(|| format!("create file {name}"))?;
            f.truncate().with_context(|| format!("truncate {name}"))?;
            let mut data = Vec::new();
            std::fs::File::open(entry.path())
                .with_context(|| format!("open source {}", entry.path().display()))?
                .read_to_end(&mut data)?;
            f.write_all(&data)
                .with_context(|| format!("write {name}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_has_efi_only_gpt_and_populated_fat() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("test.img");
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(staging.join("bin")).unwrap();
        std::fs::write(staging.join("config.yaml"), b"machine: {}\n").unwrap();
        std::fs::write(staging.join("vmlinuz"), b"kernel").unwrap();
        std::fs::write(staging.join("bin/tool"), b"t").unwrap();

        write_image(
            &img,
            2 * 1024 * 1024 * 1024,
            &staging,
            crate::arch::PartScheme::Gpt,
        )
        .unwrap();

        // GPT readable, exactly one partition, named EFI, type EFI system.
        let disk = gpt::GptConfig::new().writable(false).open(&img).unwrap();
        let parts = disk.partitions();
        assert_eq!(parts.len(), 1);
        let p = parts.values().next().unwrap();
        assert_eq!(p.name, "EFI");
        assert_eq!(p.part_type_guid, gpt::partition_types::EFI);

        // FAT region readable, files present with content, subdirs work.
        let file = std::fs::File::options().read(true).open(&img).unwrap();
        let (start, end) = (p.first_lba * 512, (p.last_lba + 1) * 512);
        let slice = fscommon::StreamSlice::new(file, start, end).unwrap();
        let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
        let names: Vec<String> = fs
            .root_dir()
            .iter()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(names.contains(&"config.yaml".to_string()), "{names:?}");
        assert!(names.contains(&"vmlinuz".to_string()));
        use std::io::Read;
        let mut buf = String::new();
        fs.root_dir()
            .open_file("config.yaml")
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "machine: {}\n");
        let sub: Vec<String> = fs
            .root_dir()
            .open_dir("bin")
            .unwrap()
            .iter()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "." && n != "..")
            .collect();
        assert_eq!(sub, vec!["tool".to_string()]);
    }

    #[test]
    fn too_small_image_errors() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("small.img");
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        let err = write_image(
            &img,
            100 * 1024 * 1024,
            &staging,
            crate::arch::PartScheme::Gpt,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("too small"),
            "error should mention 'too small': {err}"
        );
        // No usable output: an EFI-only GPT must not be readable here.
        assert!(
            gpt::GptConfig::new().writable(false).open(&img).is_err()
                || std::fs::metadata(&img).map(|m| m.len()).unwrap_or(0) == 0
        );
    }

    #[test]
    fn protective_mbr_present() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("mbr.img");
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        write_image(
            &img,
            2 * 1024 * 1024 * 1024,
            &staging,
            crate::arch::PartScheme::Gpt,
        )
        .unwrap();

        let bytes = std::fs::read(&img).unwrap();
        // Boot signature at the end of LBA0.
        assert_eq!(&bytes[510..512], &[0x55, 0xAA]);
        // First MBR partition entry's type byte: 0xEE = GPT protective.
        assert_eq!(bytes[0x1C2], 0xEE);
    }

    #[test]
    fn mbr_image_has_bootable_fat_primary_and_no_gpt() {
        use crate::arch::PartScheme;
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("pi.img");
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("config.txt"), b"arm_64bit=1\n").unwrap();

        write_image(&img, 1024 * 1024 * 1024, &staging, PartScheme::Mbr).unwrap();

        let raw = std::fs::read(&img).unwrap();
        assert_eq!(&raw[510..512], &[0x55, 0xAA], "MBR signature");
        assert_eq!(raw[446], 0x80, "partition 1 bootable");
        assert_eq!(raw[446 + 4], 0x0C, "partition 1 type FAT32 LBA");
        let lba = u32::from_le_bytes(raw[446 + 8..446 + 12].try_into().unwrap());
        let cnt = u32::from_le_bytes(raw[446 + 12..446 + 16].try_into().unwrap());
        assert_eq!(lba, 2048, "FAT starts at LBA 2048");
        assert!(cnt > 0);
        assert_ne!(raw[446 + 4], 0xEE, "not a protective GPT MBR");
        assert_ne!(&raw[512..520], b"EFI PART", "no GPT header");

        let file = std::fs::File::options().read(true).open(&img).unwrap();
        let slice = fscommon::StreamSlice::new(
            file,
            u64::from(lba) * 512,
            (u64::from(lba) + u64::from(cnt)) * 512,
        )
        .unwrap();
        let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
        let names: Vec<String> = fs
            .root_dir()
            .iter()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(names.contains(&"config.txt".to_string()), "{names:?}");
    }
}
