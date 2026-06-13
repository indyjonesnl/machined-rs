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
    /// "apk" (extracted into the initramfs rootfs) — the only kind in M7a.
    pub kind: String,
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
}
