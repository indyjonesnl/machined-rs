//! The closed `Resource` enum (typed specs) and `ResourceObject`
//! (metadata + spec) stored by the runtime.

use crate::block::{DiscoveredVolume, DiskStatus, MountStatus, VolumeStatus};
use crate::metadata::{Metadata, ResourceType};
use crate::network::{
    AddressSpec, AddressStatus, HostnameSpec, LinkSpec, LinkStatus, ResolverSpec, RouteSpec,
    RouteStatus,
};
use crate::pod_status::PodStatus;
use crate::runtime_status::RuntimeStatus;
use crate::time::TimeStatus;
use crate::upgrade_status::UpgradeStatus;

/// Spec for the loaded machine configuration, surfaced as a resource so
/// controllers reconcile against it via the normal watch path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineConfigSpec {
    /// Raw single-document YAML the config was parsed from. The typed view
    /// lives in the `config` crate (added in M1); the store only needs to
    /// hold and version the document.
    pub raw_yaml: String,
}

/// Observed state of a supervised service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceStatusSpec {
    pub service_id: String,
    pub state: ServiceState,
    pub healthy: bool,
    pub last_message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    Preparing,
    /// Waiting for dependencies to become ready.
    Waiting,
    /// Drained by a stop request.
    Stopped,
    Running,
    Finished,
    Skipped,
    Failed,
}

/// The closed set of resource specs. Each variant's payload is its typed spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resource {
    MachineConfig(MachineConfigSpec),
    ServiceStatus(ServiceStatusSpec),
    LinkSpec(LinkSpec),
    AddressSpec(AddressSpec),
    RouteSpec(RouteSpec),
    HostnameSpec(HostnameSpec),
    ResolverSpec(ResolverSpec),
    LinkStatus(LinkStatus),
    AddressStatus(AddressStatus),
    RouteStatus(RouteStatus),
    DiskStatus(DiskStatus),
    DiscoveredVolume(DiscoveredVolume),
    VolumeStatus(VolumeStatus),
    MountStatus(MountStatus),
    TimeStatus(TimeStatus),
    RuntimeStatus(RuntimeStatus),
    PodStatus(PodStatus),
    UpgradeStatus(UpgradeStatus),
}

impl Resource {
    /// The `ResourceType` discriminant for this spec.
    pub fn resource_type(&self) -> ResourceType {
        match self {
            Resource::MachineConfig(_) => ResourceType::MachineConfig,
            Resource::ServiceStatus(_) => ResourceType::ServiceStatus,
            Resource::LinkSpec(_) => ResourceType::LinkSpec,
            Resource::AddressSpec(_) => ResourceType::AddressSpec,
            Resource::RouteSpec(_) => ResourceType::RouteSpec,
            Resource::HostnameSpec(_) => ResourceType::HostnameSpec,
            Resource::ResolverSpec(_) => ResourceType::ResolverSpec,
            Resource::LinkStatus(_) => ResourceType::LinkStatus,
            Resource::AddressStatus(_) => ResourceType::AddressStatus,
            Resource::RouteStatus(_) => ResourceType::RouteStatus,
            Resource::DiskStatus(_) => ResourceType::DiskStatus,
            Resource::DiscoveredVolume(_) => ResourceType::DiscoveredVolume,
            Resource::VolumeStatus(_) => ResourceType::VolumeStatus,
            Resource::MountStatus(_) => ResourceType::MountStatus,
            Resource::TimeStatus(_) => ResourceType::TimeStatus,
            Resource::RuntimeStatus(_) => ResourceType::RuntimeStatus,
            Resource::PodStatus(_) => ResourceType::PodStatus,
            Resource::UpgradeStatus(_) => ResourceType::UpgradeStatus,
        }
    }
}

/// A stored object: identity/lifecycle metadata plus its typed spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceObject {
    pub metadata: Metadata,
    pub spec: Resource,
}

impl ResourceObject {
    /// Build a fresh object in namespace `ns` with id `id` from `spec`.
    /// The metadata's type is taken from the spec, guaranteeing they agree.
    pub fn new(ns: impl Into<String>, id: impl Into<String>, spec: Resource) -> Self {
        let typ = spec.resource_type();
        Self {
            metadata: Metadata::new(ns, typ, id),
            spec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_object_type_matches_spec() {
        let obj = ResourceObject::new(
            "runtime",
            "etcd",
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: "ok".into(),
            }),
        );
        assert_eq!(obj.metadata.typ, ResourceType::ServiceStatus);
        assert_eq!(obj.spec.resource_type(), ResourceType::ServiceStatus);
    }
}
