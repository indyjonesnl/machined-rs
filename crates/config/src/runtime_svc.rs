//! Built-in containerd service: pure construction + validation helpers.

use crate::types::{RestartPolicy, RuntimeSection, ServiceConfig};

/// The reserved id of the machined-managed runtime service.
pub const RUNTIME_SERVICE_ID: &str = "containerd";

/// The injected containerd service definition.
pub fn containerd_service(rt: &RuntimeSection) -> ServiceConfig {
    ServiceConfig {
        id: RUNTIME_SERVICE_ID.to_string(),
        command: vec![
            rt.binary.clone(),
            "--config".to_string(),
            rt.config_path.clone(),
        ],
        depends_on: Vec::new(),
        restart: RestartPolicy::Always,
    }
}

/// Minimal CRI-enabled containerd config.
pub fn containerd_config_toml(rt: &RuntimeSection) -> String {
    format!(
        "version = 2\n[grpc]\n  address = \"{}\"\n[plugins.\"io.containerd.grpc.v1.cri\"]\n",
        rt.socket
    )
}

/// The full service list the supervisor should run: the built-in runtime first
/// (unless disabled), then the user services.
pub fn effective_services(rt: &RuntimeSection, user: &[ServiceConfig]) -> Vec<ServiceConfig> {
    let mut out = Vec::with_capacity(user.len() + 1);
    if !rt.disabled {
        out.push(containerd_service(rt));
    }
    out.extend_from_slice(user);
    out
}

/// Reject user services that collide with the reserved runtime id.
pub fn validate_services(user: &[ServiceConfig]) -> Result<(), String> {
    if user.iter().any(|s| s.id == RUNTIME_SERVICE_ID) {
        return Err(format!("service id '{RUNTIME_SERVICE_ID}' is reserved"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_containerd_service_and_toml() {
        let rt = RuntimeSection::default();
        let svc = containerd_service(&rt);
        assert_eq!(svc.id, "containerd");
        assert_eq!(
            svc.command,
            vec![
                "/usr/bin/containerd",
                "--config",
                "/etc/containerd/config.toml"
            ]
        );
        assert_eq!(svc.restart, RestartPolicy::Always);
        assert!(containerd_config_toml(&rt).contains("/run/containerd/containerd.sock"));
    }

    #[test]
    fn effective_services_injects_first_unless_disabled() {
        let user = vec![ServiceConfig {
            id: "payload".into(),
            command: vec!["/bin/payload".into()],
            depends_on: vec!["containerd".into()],
            restart: Default::default(),
        }];
        let on = effective_services(&RuntimeSection::default(), &user);
        assert_eq!(on.len(), 2);
        assert_eq!(on[0].id, "containerd");

        let off = effective_services(
            &RuntimeSection {
                disabled: true,
                ..Default::default()
            },
            &user,
        );
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].id, "payload");
    }

    #[test]
    fn rejects_reserved_id() {
        let bad = vec![ServiceConfig {
            id: "containerd".into(),
            command: vec!["/bin/x".into()],
            depends_on: vec![],
            restart: Default::default(),
        }];
        assert!(validate_services(&bad).is_err());
        assert!(validate_services(&[]).is_ok());
    }
}
