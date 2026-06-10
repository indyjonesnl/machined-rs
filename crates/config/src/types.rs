//! Typed machine configuration (clean-break, single-document YAML).

use serde::Deserialize;

/// Top-level machine config document.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    #[serde(default)]
    pub machine: MachineSection,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineSection {
    /// Node hostname to apply during boot.
    #[serde(default)]
    pub hostname: Option<String>,
    /// `sysctl` key/values to apply during boot.
    #[serde(default)]
    pub sysctls: Vec<Sysctl>,
    /// Services machined supervises (the payload + helpers).
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sysctl {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    /// Unique service id.
    pub id: String,
    /// argv to exec (argv[0] is the program).
    pub command: Vec<String>,
    /// Service ids that must be Running before this one starts.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Restart policy on exit.
    #[serde(default)]
    pub restart: RestartPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Never restart (run-once).
    Never,
    /// Restart only on non-zero exit.
    #[default]
    OnFailure,
    /// Always restart.
    Always,
}
