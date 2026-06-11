//! Block-storage resources (observed state from discovery). Pure data.

/// An enumerated block device (disk).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskStatus {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub model: String,
    pub serial: String,
    pub rotational: bool,
    pub read_only: bool,
}

/// A discovered partition + its probed filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredVolume {
    pub device: String,
    pub disk: String,
    pub partition_uuid: String,
    pub partition_label: String,
    pub partition_type_guid: String,
    pub fs_type: Option<String>,
    pub fs_label: Option<String>,
    pub fs_uuid: Option<String>,
    pub size_bytes: u64,
}

/// Lifecycle phase of a managed volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumePhase {
    Provisioned,
    Failed,
}

/// A managed volume the provisioner owns (EFI / STATE / EPHEMERAL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeStatus {
    pub name: String,
    pub device: String,
    pub fs: String,
    pub label: String,
    pub phase: VolumePhase,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs() {
        let v = DiscoveredVolume {
            device: "/dev/sda1".into(),
            disk: "sda".into(),
            partition_uuid: "uuid".into(),
            partition_label: "EFI".into(),
            partition_type_guid: "guid".into(),
            fs_type: Some("vfat".into()),
            fs_label: None,
            fs_uuid: None,
            size_bytes: 512 * 2048,
        };
        assert_eq!(v.fs_type.as_deref(), Some("vfat"));
    }

    #[test]
    fn volume_status_constructs() {
        let v = VolumeStatus {
            name: "STATE".into(),
            device: "/dev/sda2".into(),
            fs: "ext4".into(),
            label: "STATE".into(),
            phase: VolumePhase::Provisioned,
        };
        assert_eq!(v.phase, VolumePhase::Provisioned);
    }
}
