//! Clean-break machine config: typed model, loader, and read-only provider.

pub mod load;
pub mod provider;
pub mod types;

pub use load::{load_from_str, ConfigError};
pub use provider::Provider;
pub use types::{
    InterfaceConfig, MachineConfig, MachineSection, NetworkSection, RestartPolicy, RouteConfig,
    ServiceConfig, Sysctl,
};
