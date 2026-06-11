//! Pure resource data model for machined-rs: metadata and the closed
//! `Resource` enum. No I/O, no async.

pub mod block;
pub mod metadata;
pub mod network;
pub mod resource;
pub mod runtime_status;
pub mod time;

pub use block::{DiscoveredVolume, DiskStatus, MountStatus, VolumePhase, VolumeStatus};
pub use metadata::{Key, Metadata, Phase, ResourceType};
pub use network::{
    AddrCidr, AddrCidrParseError, AddressSpec, AddressStatus, HostnameSpec, LinkSpec, LinkStatus,
    ResolverSpec, RouteSpec, RouteStatus,
};
pub use resource::{MachineConfigSpec, Resource, ResourceObject, ServiceState, ServiceStatusSpec};
pub use runtime_status::RuntimeStatus;
pub use time::TimeStatus;
