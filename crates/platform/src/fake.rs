//! In-memory fake platform that records operations instead of performing them.

use std::sync::Mutex;

use crate::{MountSpec, Platform, PlatformError, Result};

#[derive(Debug, Default)]
pub struct Recorded {
    pub mounts: Vec<MountSpec>,
    pub unmounts: Vec<String>,
    pub syncs: u32,
    /// Interleaved disk-op log ("sync", "unmount:<target>") so tests can pin
    /// cross-op ordering (e.g. sync-before-unmount), not just per-op order.
    pub disk_ops: Vec<String>,
    pub sysctls: Vec<(String, String)>,
    pub hostname: Option<String>,
    pub rebooted: bool,
    pub poweroff: bool,
}

#[derive(Default)]
pub struct FakePlatform {
    pub recorded: Mutex<Recorded>,
    pub cmdline: String,
    /// Targets whose plain unmount fails (busy simulation). Lazy always works.
    pub fail_unmount_targets: Mutex<Vec<String>>,
}

impl FakePlatform {
    pub fn new() -> Self {
        Self {
            recorded: Mutex::new(Recorded::default()),
            cmdline: "console=ttyS0".into(),
            fail_unmount_targets: Mutex::new(Vec::new()),
        }
    }
}

impl Platform for FakePlatform {
    fn mount(&self, spec: &MountSpec) -> Result<()> {
        self.recorded.lock().unwrap().mounts.push(spec.clone());
        Ok(())
    }
    fn set_sysctl(&self, key: &str, value: &str) -> Result<()> {
        self.recorded
            .lock()
            .unwrap()
            .sysctls
            .push((key.to_string(), value.to_string()));
        Ok(())
    }
    fn set_hostname(&self, name: &str) -> Result<()> {
        self.recorded.lock().unwrap().hostname = Some(name.to_string());
        Ok(())
    }
    fn kernel_cmdline(&self) -> Result<String> {
        Ok(self.cmdline.clone())
    }
    fn is_mounted(&self, target: &str) -> Result<bool> {
        Ok(self
            .recorded
            .lock()
            .unwrap()
            .mounts
            .iter()
            .any(|m| m.target == target))
    }
    fn unmount(&self, target: &str) -> Result<()> {
        if self
            .fail_unmount_targets
            .lock()
            .unwrap()
            .iter()
            .any(|t| t == target)
        {
            // Busy simulation: the mount stays and nothing is recorded.
            return Err(PlatformError::Mount {
                target: target.to_string(),
                message: "busy (fake)".into(),
            });
        }
        let mut rec = self.recorded.lock().unwrap();
        rec.mounts.retain(|m| m.target != target);
        rec.unmounts.push(target.to_string());
        rec.disk_ops.push(format!("unmount:{target}"));
        Ok(())
    }
    fn unmount_lazy(&self, target: &str) -> Result<()> {
        let mut rec = self.recorded.lock().unwrap();
        rec.mounts.retain(|m| m.target != target);
        rec.unmounts.push(target.to_string());
        rec.disk_ops.push(format!("unmount_lazy:{target}"));
        Ok(())
    }
    fn sync(&self) {
        let mut rec = self.recorded.lock().unwrap();
        rec.syncs += 1;
        rec.disk_ops.push("sync".to_string());
    }
    fn reboot(&self) -> Result<()> {
        self.recorded.lock().unwrap().rebooted = true;
        Ok(())
    }
    fn poweroff(&self) -> Result<()> {
        self.recorded.lock().unwrap().poweroff = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::essential_mounts;

    #[test]
    fn fake_records_mounts_and_sysctls() {
        let p = FakePlatform::new();
        p.mount_essential().unwrap();
        p.set_sysctl("net.ipv4.ip_forward", "1").unwrap();
        p.set_hostname("node-1").unwrap();

        let rec = p.recorded.lock().unwrap();
        assert_eq!(rec.mounts.len(), essential_mounts().len());
        assert_eq!(rec.mounts[0].target, "/proc");
        assert_eq!(rec.sysctls[0], ("net.ipv4.ip_forward".into(), "1".into()));
        assert_eq!(rec.hostname.as_deref(), Some("node-1"));
    }

    #[test]
    fn fake_tracks_is_mounted() {
        let p = FakePlatform::new();
        p.mount(&MountSpec {
            source: "/dev/sda2".into(),
            target: "/var".into(),
            fstype: "ext4".into(),
            flags: 0,
            data: None,
        })
        .unwrap();
        assert!(p.is_mounted("/var").unwrap());
        assert!(!p.is_mounted("/boot").unwrap());
    }

    #[test]
    fn fake_unmount_flips_is_mounted_and_records() {
        let p = FakePlatform::new();
        p.mount(&MountSpec {
            source: "/dev/x".into(),
            target: "/var".into(),
            fstype: "ext4".into(),
            flags: 0,
            data: None,
        })
        .unwrap();
        assert!(p.is_mounted("/var").unwrap());
        p.sync();
        p.unmount("/var").unwrap();
        assert!(!p.is_mounted("/var").unwrap());
        let rec = p.recorded.lock().unwrap();
        assert_eq!(rec.unmounts, vec!["/var"]);
        assert_eq!(rec.syncs, 1);
    }

    #[test]
    fn fail_target_plain_unmount_errs_lazy_succeeds() {
        let p = FakePlatform::new();
        p.mount(&MountSpec {
            source: "/dev/x".into(),
            target: "/var".into(),
            fstype: "ext4".into(),
            flags: 0,
            data: None,
        })
        .unwrap();
        p.fail_unmount_targets.lock().unwrap().push("/var".into());

        // Plain unmount fails and records NOTHING; the mount stays.
        assert!(p.unmount("/var").is_err());
        assert!(p.is_mounted("/var").unwrap());
        assert!(p.recorded.lock().unwrap().unmounts.is_empty());
        assert!(p.recorded.lock().unwrap().disk_ops.is_empty());

        // Lazy always works and flips is_mounted.
        p.unmount_lazy("/var").unwrap();
        assert!(!p.is_mounted("/var").unwrap());
        let rec = p.recorded.lock().unwrap();
        assert_eq!(rec.unmounts, vec!["/var"]);
        assert_eq!(rec.disk_ops, vec!["unmount_lazy:/var"]);
    }
}
