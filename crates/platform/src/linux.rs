//! Real privileged operations via `nix` and `/proc`. Only compiled on Linux.
//! These are exercised by VM-based integration tests, not unit tests (they
//! require root and a real kernel).

use std::fs;

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
