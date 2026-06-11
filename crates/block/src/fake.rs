//! In-memory `BlockBackend` for root-free tests.

use async_trait::async_trait;

use crate::{BlockBackend, DiskInfo, Result, VolumeInfo};

#[derive(Default)]
pub struct FakeBlockBackend {
    disks: Vec<DiskInfo>,
    volumes: Vec<VolumeInfo>,
}

impl FakeBlockBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_disk(mut self, disk: DiskInfo) -> Self {
        self.disks.push(disk);
        self
    }

    pub fn with_volume(mut self, volume: VolumeInfo) -> Self {
        self.volumes.push(volume);
        self
    }
}

#[async_trait]
impl BlockBackend for FakeBlockBackend {
    async fn list_disks(&self) -> Result<Vec<DiskInfo>> {
        Ok(self.disks.clone())
    }
    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>> {
        Ok(self.volumes.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str) -> DiskInfo {
        DiskInfo {
            name: name.into(),
            path: format!("/dev/{name}"),
            size_bytes: 1 << 30,
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
                fs_type: Some(crate::FsType::Vfat),
                fs_label: None,
                fs_uuid: None,
                size_bytes: 1 << 20,
            });
        assert_eq!(be.list_disks().await.unwrap().len(), 1);
        assert_eq!(be.list_volumes().await.unwrap()[0].disk, "sda");
    }
}
