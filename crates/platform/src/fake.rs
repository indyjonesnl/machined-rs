//! In-memory fake platform that records operations instead of performing them.

use std::sync::Mutex;

use crate::{MountSpec, Platform, Result};

#[derive(Debug, Default)]
pub struct Recorded {
    pub mounts: Vec<MountSpec>,
    pub sysctls: Vec<(String, String)>,
    pub hostname: Option<String>,
    pub rebooted: bool,
    pub poweroff: bool,
}

#[derive(Default)]
pub struct FakePlatform {
    pub recorded: Mutex<Recorded>,
    pub cmdline: String,
}

impl FakePlatform {
    pub fn new() -> Self {
        Self {
            recorded: Mutex::new(Recorded::default()),
            cmdline: "console=ttyS0".into(),
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
}
