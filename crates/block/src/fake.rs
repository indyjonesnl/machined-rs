//! In-memory `BlockBackend` + `BlockProvisioner` for root-free tests. Simulates
//! provisioning so a controller can `provision → list_volumes` and see the
//! result, and records destructive calls so tests can assert none were made.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{
    BlockBackend, BlockError, BlockProvisioner, DiskInfo, FsType, PartitionPlan, Result, VolumeInfo,
};

#[derive(Default)]
struct FakeState {
    disks: Vec<DiskInfo>,
    volumes: Vec<VolumeInfo>,
    wipes: Vec<String>,
    creates: Vec<String>,
    adds: Vec<(String, Vec<String>)>,
    formats: Vec<(String, FsType, String)>,
}

#[derive(Default)]
pub struct FakeBlockBackend {
    state: Mutex<FakeState>,
}

impl FakeBlockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_disk(self, disk: DiskInfo) -> Self {
        self.state.lock().unwrap().disks.push(disk);
        self
    }

    pub fn with_volume(self, volume: VolumeInfo) -> Self {
        self.state.lock().unwrap().volumes.push(volume);
        self
    }

    /// Test inspection: disks that were wiped.
    pub fn wipes(&self) -> Vec<String> {
        self.state.lock().unwrap().wipes.clone()
    }

    /// Test inspection: disks that had partitions created.
    pub fn creates(&self) -> Vec<String> {
        self.state.lock().unwrap().creates.clone()
    }

    /// Test inspection: (disk, [appended labels]) of each `add_partitions` call.
    pub fn adds(&self) -> Vec<(String, Vec<String>)> {
        self.state.lock().unwrap().adds.clone()
    }

    /// Test inspection: (device, fs, label) of each format call.
    pub fn formats(&self) -> Vec<(String, FsType, String)> {
        self.state.lock().unwrap().formats.clone()
    }
}

/// The bare device name from a path or name (`/dev/sda` -> `sda`).
fn disk_leaf(disk: &str) -> String {
    disk.rsplit('/').next().unwrap_or(disk).to_string()
}

fn part_device(disk: &str, num: usize) -> String {
    let sep = if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    // disk here is a /dev path; keep the path prefix.
    format!("{disk}{sep}{num}")
}

#[async_trait]
impl BlockBackend for FakeBlockBackend {
    async fn list_disks(&self) -> Result<Vec<DiskInfo>> {
        Ok(self.state.lock().unwrap().disks.clone())
    }
    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>> {
        Ok(self.state.lock().unwrap().volumes.clone())
    }
}

#[async_trait]
impl BlockProvisioner for FakeBlockBackend {
    async fn wipe(&self, disk: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.wipes.push(disk.to_string());
        let leaf = disk_leaf(disk);
        st.volumes.retain(|v| v.disk != leaf);
        Ok(())
    }

    async fn create_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>> {
        let mut st = self.state.lock().unwrap();
        st.creates.push(disk.to_string());
        // Mirror SysfsBlock: VolumeInfo.disk is the bare device name (e.g. "sda"),
        // while device is the full /dev path.
        let leaf = disk_leaf(disk);
        let mut devices = Vec::new();
        for (i, p) in plan.iter().enumerate() {
            let device = part_device(disk, i + 1);
            devices.push(device.clone());
            st.volumes.push(VolumeInfo {
                device,
                disk: leaf.clone(),
                partition_uuid: format!("uuid-{}", i + 1),
                partition_label: p.label.clone(),
                partition_type_guid: p.part_type.type_guid().to_string(),
                fs_type: None,
                fs_label: None,
                fs_uuid: None,
                size_bytes: p.size_bytes,
            });
        }
        Ok(devices)
    }

    async fn add_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>> {
        let mut st = self.state.lock().unwrap();
        let leaf = disk_leaf(disk);
        // Mirror SysfsBlock: REFUSE duplicate labels before touching anything,
        // and record only SUCCESSFUL appends (a refused call leaves no trace
        // in adds() — it wrote nothing).
        for p in plan {
            if st
                .volumes
                .iter()
                .any(|v| v.disk == leaf && v.partition_label == p.label)
            {
                return Err(BlockError::Gpt {
                    device: disk.to_string(),
                    message: format!(
                        "partition {} already exists on {disk}; refusing to append",
                        p.label
                    ),
                });
            }
        }
        st.adds.push((
            disk.to_string(),
            plan.iter().map(|p| p.label.clone()).collect(),
        ));
        // The fake doesn't track a separate partition count, so derive the
        // existing-partition count HONESTLY from the volumes already seeded on
        // this disk (mirrors how the real impl counts gdisk.partitions() before
        // appending). New partitions are numbered after that count.
        let existing = st.volumes.iter().filter(|v| v.disk == leaf).count();
        let mut devices = Vec::new();
        for (i, p) in plan.iter().enumerate() {
            let num = existing + i + 1;
            let device = part_device(disk, num);
            devices.push(device.clone());
            st.volumes.push(VolumeInfo {
                device,
                disk: leaf.clone(),
                partition_uuid: format!("uuid-{num}"),
                partition_label: p.label.clone(),
                partition_type_guid: p.part_type.type_guid().to_string(),
                fs_type: None,
                fs_label: None,
                fs_uuid: None,
                size_bytes: p.size_bytes,
            });
        }
        Ok(devices)
    }

    async fn format(&self, device: &str, fs: FsType, label: &str) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.formats.push((device.to_string(), fs, label.to_string()));
        if let Some(v) = st.volumes.iter_mut().find(|v| v.device == device) {
            v.fs_type = Some(fs);
            v.fs_label = Some(label.to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PartType;

    fn disk(name: &str) -> DiskInfo {
        DiskInfo {
            name: name.into(),
            path: format!("/dev/{name}"),
            size_bytes: 8 << 30,
            model: "FAKE".into(),
            serial: "S1".into(),
            rotational: false,
            read_only: false,
        }
    }

    #[tokio::test]
    async fn returns_seeded_disks_and_volumes() {
        let be = FakeBlockBackend::new()
            .with_disk(disk("sda"))
            .with_volume(VolumeInfo {
                device: "/dev/sda1".into(),
                disk: "sda".into(),
                partition_uuid: "u".into(),
                partition_label: "EFI".into(),
                partition_type_guid: "g".into(),
                fs_type: Some(FsType::Vfat),
                fs_label: None,
                fs_uuid: None,
                size_bytes: 1 << 20,
            });
        assert_eq!(be.list_disks().await.unwrap().len(), 1);
        assert_eq!(be.list_volumes().await.unwrap()[0].disk, "sda");
    }

    #[tokio::test]
    async fn simulates_provisioning() {
        let be = FakeBlockBackend::new();
        let plan = vec![
            PartitionPlan {
                label: "EFI".into(),
                part_type: PartType::EfiSystem,
                fs: FsType::Vfat,
                size_bytes: 512 << 20,
            },
            PartitionPlan {
                label: "STATE".into(),
                part_type: PartType::LinuxFilesystem,
                fs: FsType::Ext4,
                size_bytes: 1 << 30,
            },
        ];
        let devs = be.create_partitions("/dev/sda", &plan).await.unwrap();
        assert_eq!(devs, vec!["/dev/sda1".to_string(), "/dev/sda2".to_string()]);
        be.format("/dev/sda2", FsType::Ext4, "STATE").await.unwrap();

        let vols = be.list_volumes().await.unwrap();
        assert_eq!(vols.len(), 2);
        let state = vols.iter().find(|v| v.partition_label == "STATE").unwrap();
        assert_eq!(state.fs_type, Some(FsType::Ext4));
        assert_eq!(be.creates(), vec!["/dev/sda".to_string()]);
        assert_eq!(be.formats().len(), 1);

        be.wipe("/dev/sda").await.unwrap();
        assert!(be.list_volumes().await.unwrap().is_empty());
        assert_eq!(be.wipes(), vec!["/dev/sda".to_string()]);
    }

    #[tokio::test]
    async fn add_partitions_numbers_after_existing() {
        // A flashed image: one existing EFI partition seeded on the disk.
        let be = FakeBlockBackend::new().with_volume(VolumeInfo {
            device: "/dev/sda1".into(),
            disk: "sda".into(),
            partition_uuid: "u".into(),
            partition_label: "EFI".into(),
            partition_type_guid: "g".into(),
            fs_type: Some(FsType::Vfat),
            fs_label: None,
            fs_uuid: None,
            size_bytes: 512 << 20,
        });
        let plan = vec![
            PartitionPlan {
                label: "STATE".into(),
                part_type: PartType::LinuxFilesystem,
                fs: FsType::Ext4,
                size_bytes: 1 << 30,
            },
            PartitionPlan {
                label: "EPHEMERAL".into(),
                part_type: PartType::LinuxFilesystem,
                fs: FsType::Ext4,
                size_bytes: 0,
            },
        ];
        let devs = be.add_partitions("/dev/sda", &plan).await.unwrap();
        // Numbered AFTER the one existing partition: sda2, sda3.
        assert_eq!(devs, vec!["/dev/sda2".to_string(), "/dev/sda3".to_string()]);

        // Recorded as a single append of (disk, [labels]).
        assert_eq!(
            be.adds(),
            vec![(
                "/dev/sda".to_string(),
                vec!["STATE".to_string(), "EPHEMERAL".to_string()]
            )]
        );

        // The existing EFI is untouched; the two new volumes are now listed.
        let vols = be.list_volumes().await.unwrap();
        assert_eq!(vols.len(), 3);
        assert!(vols.iter().any(|v| v.partition_label == "EFI"));
        assert!(vols.iter().any(|v| v.device == "/dev/sda2"));
        assert!(vols.iter().any(|v| v.device == "/dev/sda3"));

        // RE-ENTRY GUARD: appending the same labels again must refuse loudly —
        // nothing written, and only the one successful append recorded.
        let err = be.add_partitions("/dev/sda", &plan).await;
        assert!(err.is_err(), "duplicate-label append must error");
        assert!(
            err.unwrap_err().to_string().contains("STATE"),
            "error names the duplicate label"
        );
        assert_eq!(be.adds().len(), 1, "failed append is not recorded");
        assert_eq!(
            be.list_volumes().await.unwrap().len(),
            3,
            "no partition was added by the refused call"
        );
    }
}
