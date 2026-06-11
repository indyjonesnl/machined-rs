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
