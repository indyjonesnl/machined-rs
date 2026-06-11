//! In-memory `BlockBackend` + `BlockProvisioner` for root-free tests. Simulates
//! provisioning so a controller can `provision → list_volumes` and see the
//! result, and records destructive calls so tests can assert none were made.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{BlockBackend, BlockProvisioner, DiskInfo, FsType, PartitionPlan, Result, VolumeInfo};

#[derive(Default)]
struct FakeState {
    disks: Vec<DiskInfo>,
    volumes: Vec<VolumeInfo>,
    wipes: Vec<String>,
    creates: Vec<String>,
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

    /// Test inspection: (device, fs, label) of each format call.
    pub fn formats(&self) -> Vec<(String, FsType, String)> {
        self.state.lock().unwrap().formats.clone()
    }
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
        st.volumes.retain(|v| v.disk != disk);
        Ok(())
    }

    async fn create_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>> {
        let mut st = self.state.lock().unwrap();
        st.creates.push(disk.to_string());
        let mut devices = Vec::new();
        for (i, p) in plan.iter().enumerate() {
            let device = part_device(disk, i + 1);
            devices.push(device.clone());
            st.volumes.push(VolumeInfo {
                device,
                disk: disk.to_string(),
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
}
