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
