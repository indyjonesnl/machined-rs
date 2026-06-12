//! Real privileged operations via `nix` and `/proc`. Only compiled on Linux.
//! These are exercised by VM-based integration tests, not unit tests (they
//! require root and a real kernel).

use std::fs;
use std::path::Path;

use nix::mount::{mount, MsFlags};
use nix::sys::reboot::{reboot, RebootMode};
use nix::unistd::sethostname;

use crate::{MountSpec, Platform, PlatformError, Result};

pub struct LinuxPlatform;

impl LinuxPlatform {
    pub fn new() -> Self {
        LinuxPlatform
    }
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for LinuxPlatform {
    fn mount(&self, spec: &MountSpec) -> Result<()> {
        // Best-effort create the mountpoint.
        let _ = fs::create_dir_all(&spec.target);
        let flags = MsFlags::from_bits_truncate(spec.flags);
        mount(
            Some(spec.source.as_str()),
            spec.target.as_str(),
            Some(spec.fstype.as_str()),
            flags,
            spec.data.as_deref(),
        )
        .map_err(|e| PlatformError::Mount {
            target: spec.target.clone(),
            message: e.to_string(),
        })
    }

    fn load_module(&self, path: &Path) -> Result<()> {
        let file = fs::File::open(path)?;
        match nix::kmod::finit_module(&file, c"", nix::kmod::ModuleInitFlags::empty()) {
            Ok(()) => Ok(()),
            // Already loaded: idempotent success. EBUSY is NOT handled:
            // loads are sequential and single-threaded pre-udev, so no
            // concurrent insert can race us.
            Err(nix::errno::Errno::EEXIST) => Ok(()),
            Err(e) => Err(PlatformError::Other(format!(
                "finit_module {}: {e}",
                path.display()
            ))),
        }
    }

    fn set_sysctl(&self, key: &str, value: &str) -> Result<()> {
        let path = format!("/proc/sys/{}", key.replace('.', "/"));
        fs::write(&path, value).map_err(|e| PlatformError::Sysctl {
            key: key.to_string(),
            message: e.to_string(),
        })
    }

    fn set_hostname(&self, name: &str) -> Result<()> {
        sethostname(name).map_err(|e| PlatformError::Other(format!("sethostname: {e}")))
    }

    fn kernel_cmdline(&self) -> Result<String> {
        Ok(fs::read_to_string("/proc/cmdline")?)
    }

    fn is_mounted(&self, target: &str) -> Result<bool> {
        // /proc/self/mountinfo field 5 (1-based) is the mount point.
        let content = match std::fs::read_to_string("/proc/self/mountinfo") {
            Ok(c) => c,
            // /proc itself not mounted yet (fresh kernel, machined as /init):
            // nothing observable is mounted — report "not mounted" so early
            // boot proceeds to mount instead of aborting into a PID1 panic.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        Ok(content
            .lines()
            .any(|line| line.split_whitespace().nth(4) == Some(target)))
    }

    fn unmount(&self, target: &str) -> Result<()> {
        nix::mount::umount2(target, nix::mount::MntFlags::empty()).map_err(|e| {
            PlatformError::Mount {
                target: target.to_string(),
                message: format!("umount: {e}"),
            }
        })
    }

    fn unmount_lazy(&self, target: &str) -> Result<()> {
        nix::mount::umount2(target, nix::mount::MntFlags::MNT_DETACH).map_err(|e| {
            PlatformError::Mount {
                target: target.to_string(),
                message: format!("umount2(MNT_DETACH): {e}"),
            }
        })
    }

    fn sync(&self) {
        nix::unistd::sync();
    }

    fn reboot(&self) -> Result<()> {
        reboot(RebootMode::RB_AUTOBOOT)
            .map(|_| ())
            .map_err(|e| PlatformError::Other(format!("reboot: {e}")))
    }

    fn poweroff(&self) -> Result<()> {
        reboot(RebootMode::RB_POWER_OFF)
            .map(|_| ())
            .map_err(|e| PlatformError::Other(format!("poweroff: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_mounted_bogus_is_not() {
        let p = LinuxPlatform::new();
        assert!(p.is_mounted("/").unwrap(), "/ must be mounted");
        assert!(!p.is_mounted("/no/such/mountpoint").unwrap());
    }

    #[test]
    fn unmount_of_missing_target_errors() {
        let p = LinuxPlatform::new();
        assert!(p.unmount("/no/such/mnt").is_err());
    }

    #[test]
    fn ms_flag_constants_match_kernel_bits() {
        // The crate's portable MS_* constants must survive
        // MsFlags::from_bits_truncate unchanged.
        for (ours, theirs) in [
            (crate::MS_RDONLY, MsFlags::MS_RDONLY),
            (crate::MS_NOSUID, MsFlags::MS_NOSUID),
            (crate::MS_NODEV, MsFlags::MS_NODEV),
        ] {
            assert_eq!(MsFlags::from_bits_truncate(ours), theirs);
        }
    }
}
