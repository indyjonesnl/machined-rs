//! Resource metadata: the COSI-style identity and lifecycle fields every
//! resource carries, independent of its typed spec.

use std::fmt;

/// The closed set of resource types known to machined-rs.
///
/// Adding a variant here forces every exhaustive match across the codebase to
/// handle it — the deliberate trade vs an open, dynamically-typed registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceType {
    MachineConfig,
    ServiceStatus,
    LinkSpec,
    AddressSpec,
    RouteSpec,
    HostnameSpec,
    ResolverSpec,
    LinkStatus,
    AddressStatus,
    RouteStatus,
    DiskStatus,
    DiscoveredVolume,
    VolumeStatus,
    MountStatus,
    TimeStatus,
    RuntimeStatus,
}

impl fmt::Display for ResourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ResourceType::MachineConfig => "MachineConfig",
            ResourceType::ServiceStatus => "ServiceStatus",
            ResourceType::LinkSpec => "LinkSpec",
            ResourceType::AddressSpec => "AddressSpec",
            ResourceType::RouteSpec => "RouteSpec",
            ResourceType::HostnameSpec => "HostnameSpec",
            ResourceType::ResolverSpec => "ResolverSpec",
            ResourceType::LinkStatus => "LinkStatus",
            ResourceType::AddressStatus => "AddressStatus",
            ResourceType::RouteStatus => "RouteStatus",
            ResourceType::DiskStatus => "DiskStatus",
            ResourceType::DiscoveredVolume => "DiscoveredVolume",
            ResourceType::VolumeStatus => "VolumeStatus",
            ResourceType::MountStatus => "MountStatus",
            ResourceType::TimeStatus => "TimeStatus",
            ResourceType::RuntimeStatus => "RuntimeStatus",
        };
        f.write_str(s)
    }
}

/// Lifecycle phase of a resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Normal steady state.
    Running,
    /// Marked for deletion; held while finalizers remain.
    TearingDown,
}

/// Fully-qualifying identity of a resource within the store.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub namespace: String,
    pub typ: ResourceType,
    pub id: String,
}

impl Key {
    pub fn new(namespace: impl Into<String>, typ: ResourceType, id: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            typ,
            id: id.into(),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.namespace, self.typ, self.id)
    }
}

/// Metadata carried by every resource object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub namespace: String,
    pub typ: ResourceType,
    pub id: String,
    /// Monotonic per-resource version, bumped on every spec mutation.
    pub version: u64,
    /// Owning controller name, if this resource is controller-managed.
    pub owner: Option<String>,
    /// Finalizer names that must be cleared before deletion completes.
    pub finalizers: Vec<String>,
    pub phase: Phase,
}

impl Metadata {
    /// Construct metadata for a freshly-created resource (version 0, Running).
    pub fn new(namespace: impl Into<String>, typ: ResourceType, id: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            typ,
            id: id.into(),
            version: 0,
            owner: None,
            finalizers: Vec::new(),
            phase: Phase::Running,
        }
    }

    pub fn key(&self) -> Key {
        Key::new(self.namespace.clone(), self.typ, self.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_new_defaults() {
        let m = Metadata::new("runtime", ResourceType::ServiceStatus, "etcd");
        assert_eq!(m.version, 0);
        assert_eq!(m.phase, Phase::Running);
        assert!(m.finalizers.is_empty());
        assert!(m.owner.is_none());
        assert_eq!(m.key().to_string(), "runtime/ServiceStatus/etcd");
    }
}
