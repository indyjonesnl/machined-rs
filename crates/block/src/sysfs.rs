//! Pure-Rust read-only block discovery: `/sys/block` enumeration + GPT reading
//! (via the `gpt` crate) + filesystem-magic probing. Linux only.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::warn;

use crate::fsprobe::probe_fs;
use crate::{BlockBackend, BlockError, DiskInfo, Result, VolumeInfo};

/// Real backend reading from `/sys` and `/dev`. Roots are injectable for tests.
pub struct SysfsBlock {
    sys_root: PathBuf,
    dev_root: PathBuf,
}

impl Default for SysfsBlock {
    fn default() -> Self {
        Self::new()
    }
}

impl SysfsBlock {
    pub fn new() -> Self {
        Self {
            sys_root: PathBuf::from("/sys"),
            dev_root: PathBuf::from("/dev"),
        }
    }

    /// Construct with explicit roots (tests point these at fixtures/tempfiles).
    pub fn with_roots(sys_root: impl Into<PathBuf>, dev_root: impl Into<PathBuf>) -> Self {
        Self {
            sys_root: sys_root.into(),
            dev_root: dev_root.into(),
        }
    }

    fn read_partitions(&self, disk: &str) -> Result<Vec<PartEntry>> {
        let path = self.dev_root.join(disk);
        let device = path.to_string_lossy().to_string();
        let cfg = gpt::GptConfig::new().writable(false);
        let gpt_disk = cfg.open(&path).map_err(|e| BlockError::Gpt {
            device: device.clone(),
            message: e.to_string(),
        })?;
        let lb = *gpt_disk.logical_block_size();
        let mut out = Vec::new();
        for (idx, part) in gpt_disk.partitions() {
            out.push(PartEntry {
                device: part_device(disk, *idx),
                uuid: part.part_guid.to_string(),
                label: part.name.clone(),
                type_guid: part.part_type_guid.guid.to_string(),
                size_bytes: part.bytes_len(lb).unwrap_or(0),
            });
        }
        Ok(out)
    }
}

struct PartEntry {
    device: String,
    uuid: String,
    label: String,
    type_guid: String,
    size_bytes: u64,
}

/// Partition device name: insert `p` before the number when the disk name ends
/// in a digit (nvme0n1 -> nvme0n1p1; sda -> sda1).
fn part_device(disk: &str, num: u32) -> String {
    let sep = if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    format!("{disk}{sep}{num}")
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_head(path: &Path, n: usize) -> Result<Vec<u8>> {
    let mut f = fs::File::open(path).map_err(|source| BlockError::Io {
        path: path.to_string_lossy().to_string(),
        source,
    })?;
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(k) => filled += k,
            Err(e) => {
                return Err(BlockError::Io {
                    path: path.to_string_lossy().to_string(),
                    source: e,
                })
            }
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

#[async_trait]
impl BlockBackend for SysfsBlock {
    async fn list_disks(&self) -> Result<Vec<DiskInfo>> {
        let block = self.sys_root.join("block");
        let entries = fs::read_dir(&block).map_err(|source| BlockError::Io {
            path: block.to_string_lossy().to_string(),
            source,
        })?;
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| BlockError::Io {
                path: block.to_string_lossy().to_string(),
                source,
            })?;
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip only pure memory-backed virtual devices. loop/dm/md are kept
            // deliberately: loop is needed by the loopback integration test, and
            // a device with no GPT is harmlessly skipped during list_volumes
            // anyway (read-only discovery). Production install disks are never
            // loop devices.
            if name.starts_with("ram") || name.starts_with("zram") {
                continue;
            }
            let dir = block.join(&name);
            let size_sectors: u64 = read_trim(&dir.join("size"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            out.push(DiskInfo {
                name: name.clone(),
                path: self.dev_root.join(&name).to_string_lossy().to_string(),
                size_bytes: size_sectors.saturating_mul(512),
                model: read_trim(&dir.join("device/model")).unwrap_or_default(),
                serial: read_trim(&dir.join("device/serial")).unwrap_or_default(),
                rotational: read_trim(&dir.join("queue/rotational")).as_deref() == Some("1"),
                read_only: read_trim(&dir.join("ro")).as_deref() == Some("1"),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>> {
        let mut out = Vec::new();
        for disk in self.list_disks().await? {
            let parts = match self.read_partitions(&disk.name) {
                Ok(p) => p,
                Err(e) => {
                    warn!(disk = %disk.name, error = %e, "skipping disk: partition read failed");
                    continue;
                }
            };
            for p in parts {
                let dev_path = self.dev_root.join(&p.device);
                let probe = read_head(&dev_path, 8192).ok().and_then(|b| probe_fs(&b));
                out.push(VolumeInfo {
                    device: dev_path.to_string_lossy().to_string(),
                    disk: disk.name.clone(),
                    partition_uuid: p.uuid,
                    partition_label: p.label,
                    partition_type_guid: p.type_guid,
                    fs_type: probe.as_ref().map(|x| x.fs_type),
                    fs_label: probe.as_ref().and_then(|x| x.label.clone()),
                    fs_uuid: probe.as_ref().and_then(|x| x.uuid.clone()),
                    size_bytes: p.size_bytes,
                });
            }
        }
        Ok(out)
    }
}

use crate::{BlockProvisioner, FsType, PartType, PartitionPlan};

impl SysfsBlock {
    fn disk_path(&self, disk: &str) -> PathBuf {
        // Accept either a bare name ("sda") or a full path ("/dev/sda").
        if disk.starts_with('/') {
            PathBuf::from(disk)
        } else {
            self.dev_root.join(disk)
        }
    }
}

/// Trigger a kernel partition-table re-read so partition device nodes appear.
fn reread_partition_table(path: &Path) -> Result<()> {
    // BLKRRPART ioctl: _IO(0x12, 95). Takes no argument; asks the kernel to
    // re-read the partition table of the block device behind `fd`.
    nix::ioctl_none!(blkrrpart, 0x12, 95);
    let f = fs::File::open(path).map_err(|source| BlockError::Io {
        path: path.to_string_lossy().to_string(),
        source,
    })?;
    use std::os::fd::AsRawFd;
    // SAFETY: BLKRRPART takes no argument and only asks the kernel to re-read
    // the partition table of the open block device.
    let res = unsafe { blkrrpart(f.as_raw_fd()) };
    res.map(|_| ()).map_err(|e| BlockError::Wipe {
        disk: path.to_string_lossy().to_string(),
        message: format!("BLKRRPART: {e}"),
    })
}

/// Register new partitions with the kernel one-by-one via BLKPG_ADD_PARTITION
/// — partprobe's method. Unlike BLKRRPART (which re-reads the WHOLE table and
/// returns EBUSY whenever any partition of the disk is open, e.g. EFI mounted
/// at /boot) BLKPG adds a single partition without disturbing its siblings.
/// `parts` is (GPT entry index, first LBA, last LBA inclusive); `lb` is the
/// logical block size in bytes.
fn blkpg_add_partitions(path: &Path, lb: u64, parts: &[(u32, u64, u64)]) -> Result<()> {
    use nix::libc;
    use std::os::fd::AsRawFd;

    // ABI from <linux/blkpg.h> — verified against the header:
    //   struct blkpg_partition { long long start; long long length; int pno;
    //                            char devname[64]; char volname[64]; };
    //   struct blkpg_ioctl_arg { int op; int flags; int datalen; void *data; };
    //   #define BLKPG _IO(0x12,105)        /* 0x1269 */
    //   #define BLKPG_ADD_PARTITION 1
    // start/length are BYTES (not sectors); devname/volname are unused/ignored.
    const BLKPG_ADD_PARTITION: libc::c_int = 1;
    #[repr(C)]
    struct BlkpgPartition {
        start: libc::c_longlong,
        length: libc::c_longlong,
        pno: libc::c_int,
        devname: [u8; 64],
        volname: [u8; 64],
    }
    #[repr(C)]
    struct BlkpgIoctlArg {
        op: libc::c_int,
        flags: libc::c_int,
        datalen: libc::c_int,
        data: *mut libc::c_void,
    }
    // Compile-time ABI pins (root-only code unit tests can't reach): values
    // taken from a C `sizeof`/`offsetof` run against <linux/blkpg.h> on x86_64
    // (start@0, length@8, pno@16, devname@20, volname@84).
    const _: () = assert!(std::mem::size_of::<BlkpgPartition>() == 152);
    const _: () = assert!(std::mem::size_of::<BlkpgIoctlArg>() == 24);
    nix::ioctl_write_ptr_bad!(blkpg, nix::request_code_none!(0x12, 105), BlkpgIoctlArg);

    let f = fs::File::open(path).map_err(|source| BlockError::Io {
        path: path.to_string_lossy().to_string(),
        source,
    })?;
    for &(pno, first_lba, last_lba) in parts {
        let mut part = BlkpgPartition {
            start: (first_lba * lb) as libc::c_longlong,
            length: ((last_lba - first_lba + 1) * lb) as libc::c_longlong,
            pno: pno as libc::c_int,
            devname: [0; 64],
            volname: [0; 64],
        };
        let arg = BlkpgIoctlArg {
            op: BLKPG_ADD_PARTITION,
            flags: 0,
            datalen: std::mem::size_of::<BlkpgPartition>() as libc::c_int,
            data: std::ptr::addr_of_mut!(part).cast(),
        };
        // SAFETY: BLKPG/ADD_PARTITION only reads the well-formed arg + payload,
        // both of which outlive the call; the fd is a valid open block device.
        let res = unsafe { blkpg(f.as_raw_fd(), &arg) };
        match res {
            Ok(_) => {}
            // Tolerated per partition: the kernel may already know this node
            // (a previous interrupted attempt) — the existence check that
            // follows in add_partitions is the real gate.
            Err(nix::errno::Errno::EBUSY) | Err(nix::errno::Errno::EEXIST) => {}
            Err(e) => {
                return Err(BlockError::Gpt {
                    device: path.to_string_lossy().to_string(),
                    message: format!("BLKPG add partition {pno}: {e}"),
                })
            }
        }
    }
    Ok(())
}

#[async_trait]
impl BlockProvisioner for SysfsBlock {
    async fn wipe(&self, disk: &str) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let path = self.disk_path(disk);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|source| BlockError::Io {
                path: path.to_string_lossy().to_string(),
                source,
            })?;
        // Zero the first and last 1 MiB (primary + backup GPT live there).
        let zeros = vec![0u8; 1024 * 1024];
        f.write_all(&zeros).map_err(|e| BlockError::Wipe {
            disk: path.to_string_lossy().to_string(),
            message: e.to_string(),
        })?;
        if let Ok(len) = f.seek(SeekFrom::End(0)) {
            if len > zeros.len() as u64 {
                let _ = f.seek(SeekFrom::End(-(zeros.len() as i64)));
                let _ = f.write_all(&zeros);
            }
        }
        f.flush().ok();
        let _ = reread_partition_table(&path);
        Ok(())
    }

    async fn create_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>> {
        let path = self.disk_path(disk);
        let device = path.to_string_lossy().to_string();
        let mut gdisk = gpt::GptConfig::new()
            .writable(true)
            .initialized(false)
            .open(&path)
            .map_err(|e| BlockError::Gpt {
                device: device.clone(),
                message: e.to_string(),
            })?;
        gdisk
            .update_partitions(std::collections::BTreeMap::new())
            .map_err(|e| BlockError::Gpt {
                device: device.clone(),
                message: e.to_string(),
            })?;
        let lb = u64::from(*gdisk.logical_block_size());
        for p in plan {
            let ptype = match p.part_type {
                PartType::EfiSystem => gpt::partition_types::EFI,
                PartType::LinuxFilesystem => gpt::partition_types::LINUX_FS,
            };
            // size 0 → use the rest: the largest free run, in bytes. INVARIANT:
            // a size-0 entry must be the LAST partition in the plan — it claims
            // all remaining free space, so any partition after it would starve
            // (and a 0-byte add_partition would error). fixed_layout() upholds
            // this (only EPHEMERAL is size 0).
            let size = if p.size_bytes == 0 {
                let free_lba = gdisk
                    .find_free_sectors()
                    .into_iter()
                    .map(|(_, len)| len)
                    .max()
                    .unwrap_or(0);
                free_lba.saturating_mul(lb)
            } else {
                p.size_bytes
            };
            gdisk
                .add_partition(&p.label, size, ptype, 0, None)
                .map_err(|e| BlockError::Gpt {
                    device: device.clone(),
                    message: e.to_string(),
                })?;
        }
        gdisk.write().map_err(|e| BlockError::Gpt {
            device: device.clone(),
            message: e.to_string(),
        })?;

        // Re-read so partition nodes appear, then derive their paths.
        let _ = reread_partition_table(&path);
        let disk_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| disk.to_string());
        Ok((1..=plan.len())
            .map(|n| {
                self.dev_root
                    .join(part_device(&disk_name, n as u32))
                    .to_string_lossy()
                    .to_string()
            })
            .collect())
    }

    async fn add_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>> {
        let path = self.disk_path(disk);
        let device = path.to_string_lossy().to_string();
        // Inner scope: GptDisk is not Send, so it must be dropped before the
        // awaits in the node-verification loop below.
        let (lb, new_parts) = {
            // Open the EXISTING table — initialized(true) — so the imager's GPT +
            // EFI entry are preserved. No update_partitions(clear): we APPEND only.
            let mut gdisk = gpt::GptConfig::new()
                .writable(true)
                .initialized(true)
                .open(&path)
                .map_err(|e| BlockError::Gpt {
                    device: device.clone(),
                    message: e.to_string(),
                })?;
            // REFUSE duplicate labels BEFORE writing anything: a re-entry against
            // stale discovery (the controller thinking STATE is still missing when
            // it was already appended) must be a loud no-op, not a fourth partition.
            for p in plan {
                if gdisk.partitions().values().any(|e| e.name == p.label) {
                    return Err(BlockError::Gpt {
                        device: device.clone(),
                        message: format!(
                            "partition {} already exists on {disk}; refusing to append",
                            p.label
                        ),
                    });
                }
            }

            let lb = u64::from(*gdisk.logical_block_size());
            // Capture each new entry's GPT index + extents (for BLKPG and for the
            // device paths) BEFORE write() — and so EFI's entry is never touched.
            let mut new_parts: Vec<(u32, u64, u64)> = Vec::new();
            for p in plan {
                let ptype = match p.part_type {
                    PartType::EfiSystem => gpt::partition_types::EFI,
                    PartType::LinuxFilesystem => gpt::partition_types::LINUX_FS,
                };
                // size 0 → use the rest: the largest free run, in bytes. INVARIANT:
                // a size-0 entry must be the LAST partition in the plan (mirrors
                // create_partitions). The append layout upholds this — only
                // EPHEMERAL is size 0.
                let size = if p.size_bytes == 0 {
                    let free_lba = gdisk
                        .find_free_sectors()
                        .into_iter()
                        .map(|(_, len)| len)
                        .max()
                        .unwrap_or(0);
                    free_lba.saturating_mul(lb)
                } else {
                    p.size_bytes
                };
                let id = gdisk
                    .add_partition(&p.label, size, ptype, 0, None)
                    .map_err(|e| BlockError::Gpt {
                        device: device.clone(),
                        message: e.to_string(),
                    })?;
                let entry = gdisk.partitions().get(&id).ok_or_else(|| BlockError::Gpt {
                    device: device.clone(),
                    message: format!("added partition {id} missing from table"),
                })?;
                new_parts.push((id, entry.first_lba, entry.last_lba));
            }
            gdisk.write().map_err(|e| BlockError::Gpt {
                device: device.clone(),
                message: e.to_string(),
            })?;
            (lb, new_parts)
        };

        // Make the kernel see the new partitions. BLKRRPART re-reads the whole
        // table but returns EBUSY whenever ANY partition of the disk is open —
        // and in THIS feature's primary scenario EFI is already mounted at
        // /boot by pid1. Fall back to per-partition BLKPG_ADD_PARTITION
        // (partprobe's method), which works with mounted siblings.
        if reread_partition_table(&path).is_err() {
            blkpg_add_partitions(&path, lb, &new_parts)?;
        }

        // Device-path numbering: the kernel names partition nodes by GPT entry
        // index, and add_partition returned exactly those indices (for an
        // append onto a gap-free table starting at 1: existing+1, existing+2,
        // …). BLKPG above registered these same pno's, so path and kernel view
        // agree by construction; should they ever diverge (exotic table with
        // holes + BLKRRPART path), the existence check below fails hard before
        // the caller can format anything.
        let disk_name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| disk.to_string());
        let devices: Vec<String> = new_parts
            .iter()
            .map(|&(id, ..)| {
                self.dev_root
                    .join(part_device(&disk_name, id))
                    .to_string_lossy()
                    .to_string()
            })
            .collect();

        // VERIFY every returned node exists before handing them to format —
        // devtmpfs node creation is fast but not synchronous with the ioctl.
        // A missing node is a HARD error: never let the caller mkfs a path
        // that may later resolve to the wrong partition.
        for dev in &devices {
            let mut present = Path::new(dev).exists();
            for _ in 0..10 {
                if present {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                present = Path::new(dev).exists();
            }
            if !present {
                return Err(BlockError::Gpt {
                    device: device.clone(),
                    message: format!(
                        "partition node {dev} did not appear after partition-table update"
                    ),
                });
            }
        }
        Ok(devices)
    }

    async fn format(&self, device: &str, fs: FsType, label: &str) -> Result<()> {
        let (prog, args): (&str, Vec<String>) = match fs {
            FsType::Ext4 => (
                "mkfs.ext4",
                vec!["-F".into(), "-L".into(), label.into(), device.into()],
            ),
            FsType::Vfat => ("mkfs.vfat", vec!["-n".into(), label.into(), device.into()]),
            FsType::Xfs => (
                "mkfs.xfs",
                vec!["-f".into(), "-L".into(), label.into(), device.into()],
            ),
            FsType::Swap => ("mkswap", vec!["-L".into(), label.into(), device.into()]),
        };
        let status = tokio::process::Command::new(prog)
            .args(&args)
            .status()
            .await
            .map_err(|e| BlockError::Mkfs {
                device: device.to_string(),
                message: format!("{prog}: {e}"),
            })?;
        if !status.success() {
            return Err(BlockError::Mkfs {
                device: device.to_string(),
                message: format!("{prog} exited {status}"),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn part_device_naming() {
        assert_eq!(part_device("sda", 1), "sda1");
        assert_eq!(part_device("nvme0n1", 2), "nvme0n1p2");
        assert_eq!(part_device("loop0", 1), "loop0p1");
    }

    #[tokio::test]
    async fn list_disks_parses_sysfs_fixture() {
        let dir = std::env::temp_dir().join(format!("mnd-sysfs-{}", std::process::id()));
        let sda = dir.join("block/sda");
        fs::create_dir_all(sda.join("device")).unwrap();
        fs::create_dir_all(sda.join("queue")).unwrap();
        let w = |p: PathBuf, v: &str| {
            let mut f = fs::File::create(p).unwrap();
            f.write_all(v.as_bytes()).unwrap();
        };
        w(sda.join("size"), "2048\n");
        w(sda.join("ro"), "0\n");
        w(sda.join("queue/rotational"), "1\n");
        w(sda.join("device/model"), "TEST MODEL\n");
        w(sda.join("device/serial"), "SER123\n");
        // A ram device that must be filtered out.
        fs::create_dir_all(dir.join("block/ram0")).unwrap();
        w(dir.join("block/ram0/size"), "100\n");

        let be = SysfsBlock::with_roots(&dir, "/dev");
        let disks = be.list_disks().await.unwrap();
        fs::remove_dir_all(&dir).ok();

        assert_eq!(disks.len(), 1, "ram0 filtered");
        let d = &disks[0];
        assert_eq!(d.name, "sda");
        assert_eq!(d.size_bytes, 2048 * 512);
        assert!(d.rotational);
        assert!(!d.read_only);
        assert_eq!(d.model, "TEST MODEL");
        assert_eq!(d.serial, "SER123");
    }

    // Reads a GPT written into a tempfile (no kernel partitions needed to read
    // the partition table itself).
    #[test]
    fn read_partitions_from_gpt_tempfile() {
        let dir = std::env::temp_dir().join(format!("mnd-gpt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let img = dir.join("sdz");

        // Create a 16 MiB image and write a GPT with two partitions.
        {
            let f = fs::File::create(&img).unwrap();
            f.set_len(16 * 1024 * 1024).unwrap();
        }
        let mut gdisk = gpt::GptConfig::new()
            .writable(true)
            .initialized(false)
            .open(&img)
            .unwrap();
        gdisk
            .update_partitions(std::collections::BTreeMap::new())
            .unwrap();
        gdisk
            .add_partition("EFI", 1024 * 1024, gpt::partition_types::EFI, 0, None)
            .unwrap();
        gdisk
            .add_partition("STATE", 1024 * 1024, gpt::partition_types::EFI, 0, None)
            .unwrap();
        gdisk.write().unwrap();

        let be = SysfsBlock::with_roots("/sys", &dir);
        let parts = be.read_partitions("sdz").unwrap();
        fs::remove_dir_all(&dir).ok();

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].device, "sdz1");
        assert_eq!(parts[1].device, "sdz2");
        assert!(parts.iter().any(|p| p.label == "EFI"));
        assert!(parts.iter().any(|p| p.label == "STATE"));
    }
}
