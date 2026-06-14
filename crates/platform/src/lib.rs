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

/// Linux MS_* mount-flag bits for `MountSpec.flags`. The real platform maps
/// these through `nix::mount::MsFlags::from_bits_truncate`, so the values must
/// match the kernel's (pinned by a test in `linux.rs`).
pub const MS_RDONLY: u64 = 0x1;
pub const MS_NOSUID: u64 = 0x2;
pub const MS_NODEV: u64 = 0x4;

/// cgroup-v2 unified hierarchy mount point.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Controllers machined delegates to the root subtree so containers can get
/// cpu/memory/pids/io cgroups. Intersected with what the kernel actually offers.
pub const CGROUP_DELEGATED: &[&str] = &["cpu", "memory", "pids", "io"];
/// Leaf cgroup PID1 moves into (cgroup-v2 "no internal processes" convention).
pub const CGROUP_INIT_LEAF: &str = "init.scope";

/// The `cgroup.subtree_control` write enabling each desired controller that is
/// actually `available` (kernel `cgroup.controllers`), in `CGROUP_DELEGATED`
/// order, each `+`-prefixed and space-joined. Pure.
pub fn subtree_control_line(available: &[&str]) -> String {
    CGROUP_DELEGATED
        .iter()
        .filter(|c| available.contains(c))
        .map(|c| format!("+{c}"))
        .collect::<Vec<_>>()
        .join(" ")
}

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
        // cgroup v2 unified hierarchy — containerd/runc need it for container
        // cgroups. (RuntimeReady itself doesn't require it, but a functional
        // runtime does; controller subtree-delegation is deferred to the
        // pod-launch milestone.)
        m("cgroup2", "/sys/fs/cgroup", "cgroup2"),
    ]
}

use std::path::Path;

/// Abstraction over the privileged operations early boot needs.
pub trait Platform: Send + Sync {
    fn mount(&self, spec: &MountSpec) -> Result<()>;
    /// Load a kernel module from an absolute `.ko` path. Already-loaded is Ok.
    fn load_module(&self, path: &Path) -> Result<()>;
    fn set_sysctl(&self, key: &str, value: &str) -> Result<()>;
    fn set_hostname(&self, name: &str) -> Result<()>;
    /// Move PID1 into a leaf cgroup and delegate controllers to the root
    /// subtree so containers get cpu/memory/pids/io cgroups (cgroup-v2). A
    /// no-op when `/sys/fs/cgroup` is not a cgroup-v2 mount.
    fn delegate_cgroups(&self) -> Result<()>;
    fn kernel_cmdline(&self) -> Result<String>;
    /// Whether something is currently mounted at `target`.
    fn is_mounted(&self, target: &str) -> Result<bool>;
    /// Unmount the filesystem at `target`.
    fn unmount(&self, target: &str) -> Result<()>;
    /// Lazily unmount `target` (detach now, clean up when no longer busy).
    fn unmount_lazy(&self, target: &str) -> Result<()>;
    /// Flush filesystem buffers to disk.
    fn sync(&self);
    fn reboot(&self) -> Result<()>;
    fn poweroff(&self) -> Result<()>;

    /// Mount every essential pseudo-filesystem that isn't already mounted.
    /// Idempotent: re-running (e.g. the sequencer's MountFilesystems after an
    /// early image-boot mount) is a no-op for already-present mounts.
    ///
    /// An is_mounted error means "unknown → attempt the mount": on a fresh
    /// kernel (machined as /init) /proc isn't mounted yet, so mountinfo is
    /// unreadable. The worst case of mounting anyway is a harmless stacked
    /// pseudo-fs mount — infinitely better than a PID1 that mounts nothing
    /// and panics the kernel.
    fn mount_essential(&self) -> Result<()> {
        for spec in essential_mounts() {
            if !self.is_mounted(&spec.target).unwrap_or(false) {
                self.mount(&spec)?;
            }
        }
        Ok(())
    }
}
