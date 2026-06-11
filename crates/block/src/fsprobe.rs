//! Filesystem magic-byte probing. Filled in Task 3.

/// Probed filesystem identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsProbe {
    pub fs_type: crate::FsType,
    pub label: Option<String>,
    pub uuid: Option<String>,
}

/// Probe a filesystem from the leading bytes of a device. Filled in Task 3.
pub fn probe_fs(_buf: &[u8]) -> Option<FsProbe> {
    None
}
