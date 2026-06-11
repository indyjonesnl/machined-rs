//! Block-device discovery backend: a `BlockBackend` trait abstracting disk and
//! partition enumeration, with a pure-Rust `SysfsBlock` implementation (Linux)
//! and an in-memory fake. Read-only in M2b-1.

pub mod fake;
pub mod fsprobe;
#[cfg(target_os = "linux")]
pub mod sysfs;

use async_trait::async_trait;

pub use fake::FakeBlockBackend;
pub use fsprobe::{probe_fs, FsProbe};
#[cfg(target_os = "linux")]
pub use sysfs::SysfsBlock;

#[derive(thiserror::Error, Debug)]
pub enum BlockError {
    #[error("io {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("gpt {device}: {message}")]
    Gpt { device: String, message: String },
    #[error("wipe {disk}: {message}")]
    Wipe { disk: String, message: String },
    #[error("mkfs {device}: {message}")]
    Mkfs { device: String, message: String },
}

pub type Result<T> = std::result::Result<T, BlockError>;

/// Probed filesystem type (the set M2b-1 recognises).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsType {
    Ext4,
    Vfat,
    Xfs,
    Swap,
}

impl FsType {
    pub fn as_str(self) -> &'static str {
        match self {
            FsType::Ext4 => "ext4",
            FsType::Vfat => "vfat",
            FsType::Xfs => "xfs",
            FsType::Swap => "swap",
        }
    }
}

/// An enumerated disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskInfo {
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
pub struct VolumeInfo {
    pub device: String,
    pub disk: String,
    pub partition_uuid: String,
    pub partition_label: String,
    pub partition_type_guid: String,
    pub fs_type: Option<FsType>,
    pub fs_label: Option<String>,
    pub fs_uuid: Option<String>,
    pub size_bytes: u64,
}

/// Read-only enumeration of disks and their partitions/filesystems.
#[async_trait]
pub trait BlockBackend: Send + Sync {
    async fn list_disks(&self) -> Result<Vec<DiskInfo>>;
    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>>;
}

/// GPT partition type (the two types this OS lays out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartType {
    EfiSystem,
    LinuxFilesystem,
}

impl PartType {
    /// The GPT type GUID string for this partition type.
    pub fn type_guid(self) -> &'static str {
        match self {
            PartType::EfiSystem => "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
            PartType::LinuxFilesystem => "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
        }
    }
}

/// A planned partition. `size_bytes == 0` means "use the remaining space".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionPlan {
    pub label: String,
    pub part_type: PartType,
    pub fs: FsType,
    pub size_bytes: u64,
}

/// Destructive disk provisioning. A supertrait of [`BlockBackend`] so a
/// provisioner can also discover, while read-only backends need not implement
/// these. ALL three operations are idempotent from the caller's perspective:
/// re-creating the same layout / re-formatting an already-correct device
/// converges.
#[async_trait]
pub trait BlockProvisioner: BlockBackend {
    /// Destroy the partition table on `disk` (zap primary + backup GPT).
    async fn wipe(&self, disk: &str) -> Result<()>;
    /// Write a fresh GPT with `plan`; return the created partition device paths.
    async fn create_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>>;
    /// Create a filesystem on `device` with `label`.
    async fn format(&self, device: &str, fs: FsType, label: &str) -> Result<()>;
}
