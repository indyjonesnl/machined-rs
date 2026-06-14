//! Clean-break machine config: typed model, loader, and read-only provider.

pub mod load;
pub mod provider;
pub mod runtime_svc;
pub mod types;

pub use load::{load_from_str, ConfigError};
pub use provider::Provider;
pub use runtime_svc::{
    containerd_config_toml, containerd_service, ctr_import_args, effective_services,
    validate_services, PAUSE_IMAGE, RUNTIME_SERVICE_ID,
};
pub use types::{
    InstallSection, InterfaceConfig, MachineConfig, MachineSection, NetworkSection, PodConfig,
    RestartPolicy, RouteConfig, RuntimeSection, ServiceConfig, Sysctl, TimeSection,
};
