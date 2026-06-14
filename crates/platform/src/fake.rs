//! In-memory fake platform that records operations instead of performing them.

use std::path::Path;
use std::sync::Mutex;

use crate::{MountSpec, Platform, PlatformError, Result};

#[derive(Debug, Default)]
pub struct Recorded {
    pub mounts: Vec<MountSpec>,
    pub unmounts: Vec<String>,
    /// Absolute `.ko` paths passed to `load_module`, in call order.
    pub modules: Vec<String>,
    pub syncs: u32,
    /// Interleaved disk-op log ("sync", "unmount:<target>") so tests can pin
    /// cross-op ordering (e.g. sync-before-unmount), not just per-op order.
    pub disk_ops: Vec<String>,
    pub sysctls: Vec<(String, String)>,
    pub hostname: Option<String>,
    pub rebooted: bool,
    pub poweroff: bool,
    pub cgroup_delegated: bool,
}

#[derive(Default)]
pub struct FakePlatform {
    pub recorded: Mutex<Recorded>,
    pub cmdline: String,
    /// Targets whose plain unmount fails (busy simulation). Lazy always works.
    pub fail_unmount_targets: Mutex<Vec<String>>,
    /// Targets whose `is_mounted` errors (unreadable-mountinfo simulation,
    /// e.g. /proc not yet mounted on a fresh kernel).
    pub fail_is_mounted_targets: Mutex<Vec<String>>,
}

impl FakePlatform {
    pub fn new() -> Self {
        Self {
            recorded: Mutex::new(Recorded::default()),
            cmdline: "console=ttyS0".into(),
            fail_unmount_targets: Mutex::new(Vec::new()),
            fail_is_mounted_targets: Mutex::new(Vec::new()),
        }
    }

    /// Test inspection: kernel modules loaded, in call order.
    pub fn modules_loaded(&self) -> Vec<String> {
        self.recorded.lock().unwrap().modules.clone()
    }

    /// Mounts issued, in call order (test inspection).
    pub fn mounts(&self) -> Vec<MountSpec> {
        self.recorded.lock().unwrap().mounts.clone()
    }
}

impl Platform for FakePlatform {
    fn mount(&self, spec: &MountSpec) -> Result<()> {
        self.recorded.lock().unwrap().mounts.push(spec.clone());
        Ok(())
    }
    fn load_module(&self, path: &Path) -> Result<()> {
        self.recorded
            .lock()
            .unwrap()
            .modules
            .push(path.to_string_lossy().to_string());
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
    fn delegate_cgroups(&self) -> Result<()> {
        self.recorded.lock().unwrap().cgroup_delegated = true;
        Ok(())
    }
    fn kernel_cmdline(&self) -> Result<String> {
        Ok(self.cmdline.clone())
    }
    fn is_mounted(&self, target: &str) -> Result<bool> {
        if self
            .fail_is_mounted_targets
            .lock()
            .unwrap()
            .iter()
            .any(|t| t == target)
        {
            return Err(PlatformError::Other(format!(
                "is_mounted {target}: mountinfo unreadable (fake)"
            )));
        }
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
    fn mount_essential_skips_already_mounted() {
        let p = FakePlatform::new();
        p.mount(&essential_mounts()[0]).unwrap(); // /proc pre-mounted
        p.mount_essential().unwrap();
        let rec = p.recorded.lock().unwrap();
        // /proc recorded exactly once, not twice.
        assert_eq!(rec.mounts.iter().filter(|m| m.target == "/proc").count(), 1);
        assert_eq!(rec.mounts.len(), essential_mounts().len());
    }

    #[test]
    fn mount_essential_mounts_everything_when_is_mounted_errors() {
        // Fresh-kernel simulation: /proc is not mounted yet, so the mountinfo
        // read behind is_mounted fails. mount_essential must still mount
        // EVERYTHING — a PID1 that mounts nothing panics the kernel.
        let p = FakePlatform::new();
        p.fail_is_mounted_targets
            .lock()
            .unwrap()
            .push("/proc".into());
        p.mount_essential().unwrap();
        let rec = p.recorded.lock().unwrap();
        assert_eq!(rec.mounts.len(), essential_mounts().len());
        assert_eq!(rec.mounts[0].target, "/proc");
    }

    #[test]
    fn essential_mounts_include_cgroup2() {
        let p = FakePlatform::new();
        p.mount_essential().unwrap();
        let m = p.mounts();
        assert!(
            m.iter()
                .any(|s| s.target == "/sys/fs/cgroup" && s.fstype == "cgroup2"),
            "cgroup2 must be mounted at /sys/fs/cgroup: {m:?}"
        );
        // /sys is still mounted before cgroup (cgroup2 lives under it).
        let sys = m.iter().position(|s| s.target == "/sys");
        let cg = m.iter().position(|s| s.target == "/sys/fs/cgroup");
        assert!(sys < cg, "/sys must mount before /sys/fs/cgroup");
    }

    #[test]
    fn fake_records_module_loads() {
        let p = FakePlatform::new();
        p.load_module(std::path::Path::new("/lib/modules/x/ext4.ko"))
            .unwrap();
        assert_eq!(
            p.modules_loaded(),
            vec!["/lib/modules/x/ext4.ko".to_string()]
        );
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

    #[test]
    fn subtree_line_intersects_desired_with_available() {
        use crate::subtree_control_line;
        // All desired available → all, in desired order, each + prefixed.
        assert_eq!(subtree_control_line(&["cpu", "memory", "pids", "io"]), "+cpu +memory +pids +io");
        // io unavailable → dropped, rest kept.
        assert_eq!(subtree_control_line(&["cpu", "memory", "pids"]), "+cpu +memory +pids");
        // an unrelated available controller is ignored.
        assert_eq!(subtree_control_line(&["cpu", "rdma"]), "+cpu");
    }

    #[test]
    fn fake_records_cgroup_delegation() {
        let p = FakePlatform::new();
        p.delegate_cgroups().unwrap();
        assert!(p.recorded.lock().unwrap().cgroup_delegated);
    }
}
