//! Loading machine config from YAML and projecting it into a resource.

use std::path::Path;

use machined_resources::{MachineConfigSpec, Resource, ResourceObject};

use crate::types::MachineConfig;

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("reading config {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Parse(#[from] serde_yaml::Error),
}

/// Parse a machine config from a YAML string.
pub fn load_from_str(yaml: &str) -> Result<MachineConfig, ConfigError> {
    Ok(serde_yaml::from_str(yaml)?)
}

/// Read and parse a machine config from disk.
pub fn load_from_path(path: &Path) -> Result<(MachineConfig, String), ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let cfg = load_from_str(&raw)?;
    Ok((cfg, raw))
}

/// Build the `MachineConfig` resource object that controllers reconcile against.
/// `raw_yaml` is the document the config was parsed from.
pub fn to_resource(raw_yaml: String) -> ResourceObject {
    ResourceObject::new(
        "runtime",
        "machine-config",
        Resource::MachineConfig(MachineConfigSpec { raw_yaml }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RestartPolicy;

    const SAMPLE: &str = r#"
machine:
  hostname: node-1
  sysctls:
    - key: net.ipv4.ip_forward
      value: "1"
  services:
    - id: payload
      command: ["/usr/bin/rusternetes", "--all-in-one"]
      restart: always
"#;

    #[test]
    fn parses_machine_config() {
        let cfg = load_from_str(SAMPLE).unwrap();
        assert_eq!(cfg.machine.hostname.as_deref(), Some("node-1"));
        assert_eq!(cfg.machine.sysctls.len(), 1);
        assert_eq!(cfg.machine.services.len(), 1);
        let svc = &cfg.machine.services[0];
        assert_eq!(svc.id, "payload");
        assert_eq!(svc.restart, RestartPolicy::Always);
        assert_eq!(svc.command[0], "/usr/bin/rusternetes");
    }

    #[test]
    fn empty_config_is_valid() {
        let cfg = load_from_str("{}").unwrap();
        assert!(cfg.machine.hostname.is_none());
        assert!(cfg.machine.services.is_empty());
    }

    #[test]
    fn builds_resource_object() {
        let obj = to_resource("machine: {}".into());
        assert_eq!(obj.metadata.id, "machine-config");
        match obj.spec {
            Resource::MachineConfig(s) => assert_eq!(s.raw_yaml, "machine: {}"),
            _ => panic!("wrong resource type"),
        }
    }
}
