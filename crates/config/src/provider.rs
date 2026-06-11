//! Read-only view over the machine config handed to controllers and tasks.

use crate::types::{InstallSection, MachineConfig, NetworkSection, ServiceConfig, Sysctl};

/// A read-only, cloneable snapshot view of the loaded config.
#[derive(Clone, Debug)]
pub struct Provider {
    config: MachineConfig,
}

impl Provider {
    pub fn new(config: MachineConfig) -> Self {
        Self { config }
    }

    pub fn hostname(&self) -> Option<&str> {
        self.config.machine.hostname.as_deref()
    }

    pub fn sysctls(&self) -> &[Sysctl] {
        &self.config.machine.sysctls
    }

    pub fn services(&self) -> &[ServiceConfig] {
        &self.config.machine.services
    }

    pub fn network(&self) -> &NetworkSection {
        &self.config.machine.network
    }

    pub fn install(&self) -> Option<&InstallSection> {
        self.config.machine.install.as_ref()
    }
}
