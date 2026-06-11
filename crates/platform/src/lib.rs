//! Privileged OS operations behind a trait, so the sequencer and supervisor
//! are testable without root. `LinuxPlatform` does the real work;
//! `FakePlatform` records calls.

pub mod fake;
#[cfg(target_os = "linux")]
pub mod linux;

pub use fake::FakePlatform;
#[cfg(target_os = "linux")]
pub use linux::LinuxPlatform;

#[derive(thiserror::Error, Debug)]
pub enum PlatformError {
    #[error("mount {target}: {message}")]
    Mount { target: String, message: String },
    #[error("sysctl {key}: {message}")]
    Sysctl { key: String, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, PlatformError>;

/// A filesystem to mount during early boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountSpec {
    pub source: String,
    pub target: String,
    pub fstype: String,
    pub flags: u64,
    pub data: Option<String>,
}

/// The set of pseudo-filesystems machined mounts before anything else.
pub fn essential_mounts() -> Vec<MountSpec> {
    let m = |source: &str, target: &str, fstype: &str| MountSpec {
        source: source.into(),
        target: target.into(),
        fstype: fstype.into(),
        flags: 0,
        data: None,
    };
    vec![
        m("proc", "/proc", "proc"),
        m("sysfs", "/sys", "sysfs"),
        m("devtmpfs", "/dev", "devtmpfs"),
        m("tmpfs", "/run", "tmpfs"),
        m("tmpfs", "/tmp", "tmpfs"),
    ]
}

/// Abstraction over the privileged operations early boot needs.
pub trait Platform: Send + Sync {
    fn mount(&self, spec: &MountSpec) -> Result<()>;
    fn set_sysctl(&self, key: &str, value: &str) -> Result<()>;
    fn set_hostname(&self, name: &str) -> Result<()>;
    fn kernel_cmdline(&self) -> Result<String>;
    /// Whether something is currently mounted at `target`.
    fn is_mounted(&self, target: &str) -> Result<bool>;
    fn reboot(&self) -> Result<()>;
    fn poweroff(&self) -> Result<()>;

    /// Mount every essential pseudo-filesystem. Default impl loops `mount`.
    fn mount_essential(&self) -> Result<()> {
        for spec in essential_mounts() {
            self.mount(&spec)?;
        }
        Ok(())
    }
}
