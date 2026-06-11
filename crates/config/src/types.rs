//! Typed machine configuration (clean-break, single-document YAML).

use serde::Deserialize;
use std::net::IpAddr;

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
    /// Node network configuration.
    #[serde(default)]
    pub network: NetworkSection,
    /// Disk installation target + wipe policy.
    #[serde(default)]
    pub install: Option<InstallSection>,
    /// Time-sync configuration.
    #[serde(default)]
    pub time: TimeSection,
    /// Container runtime (containerd) management.
    #[serde(default)]
    pub runtime: RuntimeSection,
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

/// Static node network configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    #[serde(default)]
    pub interfaces: Vec<InterfaceConfig>,
    #[serde(default)]
    pub nameservers: Vec<IpAddr>,
    #[serde(default)]
    pub search: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceConfig {
    pub name: String,
    /// Admin state; defaults to up.
    #[serde(default = "default_true")]
    pub up: bool,
    #[serde(default)]
    pub mtu: Option<u32>,
    /// Addresses in `ip/prefix` form (parsed by the network controller).
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Destination CIDR; `None`/absent or `0.0.0.0/0` means default route.
    #[serde(default)]
    pub to: Option<String>,
    /// Gateway IP.
    pub via: IpAddr,
    #[serde(default)]
    pub metric: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallSection {
    /// The disk to provision, e.g. `/dev/sda`.
    pub disk: String,
    /// Wipe foreign data on the disk when provisioning. Defaults to false.
    #[serde(default)]
    pub wipe: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSection {
    /// NTP servers to query, in order. Empty → the controller's default pool.
    #[serde(default)]
    pub servers: Vec<String>,
    /// Disable time sync entirely.
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RuntimeSection {
    /// Disable runtime management entirely.
    pub disabled: bool,
    /// containerd binary path.
    pub binary: String,
    /// CRI unix socket path.
    pub socket: String,
    /// Generated containerd config path.
    pub config_path: String,
}

impl Default for RuntimeSection {
    fn default() -> Self {
        Self {
            disabled: false,
            binary: "/usr/bin/containerd".into(),
            socket: "/run/containerd/containerd.sock".into(),
            config_path: "/etc/containerd/config.toml".into(),
        }
    }
}
