//! Map the closed `Resource` enum to gRPC field lists, and parse a type name.

use machined_resources::{Resource, ResourceType};

/// Parse a `ResourceType` from its `Display` name (reverse of `Display`).
pub fn parse_resource_type(s: &str) -> Option<ResourceType> {
    Some(match s {
        "MachineConfig" => ResourceType::MachineConfig,
        "ServiceStatus" => ResourceType::ServiceStatus,
        "LinkSpec" => ResourceType::LinkSpec,
        "AddressSpec" => ResourceType::AddressSpec,
        "RouteSpec" => ResourceType::RouteSpec,
        "HostnameSpec" => ResourceType::HostnameSpec,
        "ResolverSpec" => ResourceType::ResolverSpec,
        "LinkStatus" => ResourceType::LinkStatus,
        "AddressStatus" => ResourceType::AddressStatus,
        "RouteStatus" => ResourceType::RouteStatus,
        "DiskStatus" => ResourceType::DiskStatus,
        "DiscoveredVolume" => ResourceType::DiscoveredVolume,
        "VolumeStatus" => ResourceType::VolumeStatus,
        "MountStatus" => ResourceType::MountStatus,
        "TimeStatus" => ResourceType::TimeStatus,
        "RuntimeStatus" => ResourceType::RuntimeStatus,
        "PodStatus" => ResourceType::PodStatus,
        "UpgradeStatus" => ResourceType::UpgradeStatus,
        _ => return None,
    })
}

fn kv(k: &str, v: impl ToString) -> (String, String) {
    (k.to_string(), v.to_string())
}

fn opt(v: &Option<impl ToString>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_default()
}

/// Render a resource's spec as a list of `(key, value)` fields for the API.
/// Exhaustive over the closed `Resource` enum — a new variant is a compile error.
pub fn resource_to_fields(spec: &Resource) -> Vec<(String, String)> {
    match spec {
        Resource::MachineConfig(c) => vec![kv("bytes", c.raw_yaml.len())],
        Resource::ServiceStatus(s) => vec![
            kv("service_id", &s.service_id),
            kv("state", format!("{:?}", s.state)),
            kv("healthy", s.healthy),
            kv("message", &s.last_message),
        ],
        Resource::LinkSpec(l) => vec![kv("name", &l.name), kv("up", l.up), kv("mtu", opt(&l.mtu))],
        Resource::AddressSpec(a) => vec![kv("link", &a.link), kv("address", a.address)],
        Resource::RouteSpec(r) => vec![
            kv("link", &r.link),
            kv("destination", opt(&r.destination)),
            kv("gateway", opt(&r.gateway)),
            kv("metric", r.metric),
        ],
        Resource::HostnameSpec(h) => vec![kv("hostname", &h.hostname)],
        Resource::ResolverSpec(r) => vec![
            kv("nameservers", r.nameservers.len()),
            kv("search", r.search.join(",")),
        ],
        Resource::LinkStatus(l) => vec![
            kv("name", &l.name),
            kv("up", l.up),
            kv("mtu", l.mtu),
            kv("mac", &l.mac),
        ],
        Resource::AddressStatus(a) => vec![kv("link", &a.link), kv("address", a.address)],
        Resource::RouteStatus(r) => vec![
            kv("link", &r.link),
            kv("destination", opt(&r.destination)),
            kv("gateway", opt(&r.gateway)),
        ],
        Resource::DiskStatus(d) => vec![
            kv("name", &d.name),
            kv("path", &d.path),
            kv("size_bytes", d.size_bytes),
            kv("model", &d.model),
            kv("rotational", d.rotational),
            kv("read_only", d.read_only),
        ],
        Resource::DiscoveredVolume(v) => vec![
            kv("device", &v.device),
            kv("disk", &v.disk),
            kv("partition_label", &v.partition_label),
            kv("fs_type", opt(&v.fs_type)),
            kv("size_bytes", v.size_bytes),
        ],
        Resource::VolumeStatus(v) => vec![
            kv("name", &v.name),
            kv("device", &v.device),
            kv("fs", &v.fs),
            kv("label", &v.label),
            kv("phase", format!("{:?}", v.phase)),
        ],
        Resource::MountStatus(m) => vec![
            kv("volume", &m.volume),
            kv("source", &m.source),
            kv("target", &m.target),
            kv("fstype", &m.fstype),
            kv("mounted", m.mounted),
        ],
        Resource::TimeStatus(t) => vec![
            kv("synced", t.synced),
            kv("server", &t.server),
            kv("offset_ns", t.offset_ns),
            kv("sync_count", t.sync_count),
        ],
        Resource::RuntimeStatus(r) => vec![
            kv("ready", r.ready),
            kv("name", &r.name),
            kv("version", &r.version),
        ],
        Resource::PodStatus(p) => vec![
            kv("name", &p.name),
            kv("phase", format!("{:?}", p.phase)),
            kv("container_id", &p.container_id),
            kv("pod_ip", &p.pod_ip),
            kv("message", &p.message),
        ],
        Resource::UpgradeStatus(u) => vec![
            kv("phase", format!("{:?}", u.phase)),
            kv("message", &u.message),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{Resource, ServiceState, ServiceStatusSpec, TimeStatus};

    #[test]
    fn type_name_round_trips_display() {
        for t in [
            ResourceType::ServiceStatus,
            ResourceType::DiskStatus,
            ResourceType::TimeStatus,
            ResourceType::MountStatus,
        ] {
            assert_eq!(parse_resource_type(&t.to_string()), Some(t));
        }
        assert_eq!(parse_resource_type("Nonsense"), None);
    }

    #[test]
    fn maps_fields() {
        let svc = Resource::ServiceStatus(ServiceStatusSpec {
            service_id: "etcd".into(),
            state: ServiceState::Running,
            healthy: true,
            last_message: "ok".into(),
        });
        let f = resource_to_fields(&svc);
        assert!(f.contains(&("service_id".to_string(), "etcd".to_string())));
        assert!(f.contains(&("healthy".to_string(), "true".to_string())));

        let t = Resource::TimeStatus(TimeStatus {
            synced: true,
            server: "a".into(),
            offset_ns: -5,
            sync_count: 2,
        });
        assert!(resource_to_fields(&t).contains(&("offset_ns".to_string(), "-5".to_string())));
    }
}
