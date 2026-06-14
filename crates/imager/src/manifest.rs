//! The pinned-artifact manifest (artifacts.toml): every external input to an
//! image is named here with URL + sha256. Nothing unpinned is ever downloaded.

use anyhow::Context;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// arch → artifact list.
    pub artifact: BTreeMap<String, Vec<Artifact>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub url: String,
    pub sha256: String,
    /// "apk" → initramfs rootfs; "boot-tarball" → /boot/bin (bin/* from a
    /// single .tar.gz); "boot-binary" → /boot/bin/<rename|name>;
    /// "oci-image" → /boot/images/<rename|name> (a pre-baked OCI archive);
    /// "cni-plugins" → /boot/cni/bin/{bridge,host-local,loopback} (from a
    /// cni-plugins-*.tgz).
    pub kind: String,
    /// For "boot-binary": the filename to stage as (e.g. runc). Ignored otherwise.
    #[serde(default)]
    pub rename: Option<String>,
}

impl Manifest {
    /// Load and parse a manifest from `path`.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read or is not valid manifest TOML.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing manifest {}", path.display()))
    }
    pub fn for_arch(&self, arch: &str) -> Option<&[Artifact]> {
        self.artifact.get(arch).map(|v| v.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_and_selects_arch() {
        let m: Manifest = toml::from_str(
            r#"
[[artifact.x86_64]]
name = "linux-virt"
url = "https://example.org/linux-virt.apk"
sha256 = "aa"
kind = "apk"
"#,
        )
        .unwrap();
        let arts = m.for_arch("x86_64").unwrap();
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].name, "linux-virt");
        assert!(m.for_arch("riscv").is_none());
    }

    #[test]
    fn real_artifacts_manifest_parses() {
        // The committed manifest must always parse, and carry the M7b-2 boot binaries.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("artifacts.toml");
        let m = Manifest::load(&path).expect("artifacts.toml parses");
        let x86 = m.for_arch("x86_64").expect("x86_64 arch present");
        assert!(x86
            .iter()
            .any(|a| a.name == "containerd" && a.kind == "boot-tarball"));
        assert!(x86.iter().any(|a| a.name == "runc"
            && a.kind == "boot-binary"
            && a.rename.as_deref() == Some("runc")));
        // The apk artifacts (kernel etc.) are still there.
        assert!(x86.iter().any(|a| a.kind == "apk"));
        // aarch64 section present with the same shape (apk kernel + arm64 runtime).
        let arm = m.for_arch("aarch64").expect("aarch64 arch present");
        assert!(arm
            .iter()
            .any(|a| a.name == "linux-virt" && a.kind == "apk"));
        assert!(arm
            .iter()
            .any(|a| a.name == "containerd" && a.kind == "boot-tarball"));
        assert!(arm.iter().any(|a| a.name == "runc"
            && a.kind == "boot-binary"
            && a.rename.as_deref() == Some("runc")));
        // aarch64-rpi section: Pi kernel + GPU firmware apks + arm64 runtime.
        let rpi = m.for_arch("aarch64-rpi").expect("aarch64-rpi arch present");
        assert!(rpi.iter().any(|a| a.name == "linux-rpi" && a.kind == "apk"));
        assert!(rpi
            .iter()
            .any(|a| a.name == "raspberrypi-bootloader" && a.kind == "apk"));
        assert!(rpi
            .iter()
            .any(|a| a.name == "raspberrypi-bootloader-common" && a.kind == "apk"));
        assert!(rpi.iter().any(|a| a.name == "runc"
            && a.kind == "boot-binary"
            && a.rename.as_deref() == Some("runc")));
    }
}
