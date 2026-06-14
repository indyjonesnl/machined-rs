//! Built-in containerd service: pure construction + validation helpers.

use crate::types::{RestartPolicy, RuntimeSection, ServiceConfig};

/// The reserved id of the machined-managed runtime service.
pub const RUNTIME_SERVICE_ID: &str = "containerd";

/// The pre-baked pause image the containerd CRI uses for pod sandboxes. Staged
/// on /boot/images and imported at boot (see ctr_import_args). containerd-specific.
pub const PAUSE_IMAGE: &str = "registry.k8s.io/pause:3.10";

/// `ctr` argv that imports a pre-baked OCI archive into the k8s.io namespace the
/// CRI plugin reads. containerd-specific — swapping the CRI runtime swaps this.
pub fn ctr_import_args<'a>(socket: &'a str, tar: &'a str) -> Vec<&'a str> {
    vec!["--address", socket, "-n", "k8s.io", "images", "import", tar]
}

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
        stop_grace_secs: None,
    }
}

/// Generate a functional containerd 2.x CRI config (schema version 3). Enables
/// the CRI runtime plugin (built-in + enabled by default in the upstream static
/// tarball), the runc runtime via the v2 shim with the cgroupfs driver (no
/// systemd on this node), and an explicit runc path on the boot partition.
/// `root` is persistent (EPHEMERAL → /var); `state` is volatile (/run tmpfs).
pub fn containerd_config_toml(rt: &RuntimeSection) -> String {
    format!(
        r#"version = 3
root = "/var/lib/containerd"
state = "/run/containerd"

[grpc]
  address = "{socket}"

[plugins.'io.containerd.cri.v1.runtime']
  sandbox_image = "{pause}"
  [plugins.'io.containerd.cri.v1.runtime'.containerd]
    default_runtime_name = "runc"

    [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc]
      runtime_type = "io.containerd.runc.v2"

      [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc.options]
        SystemdCgroup = false
        BinaryName = "/boot/bin/runc"
"#,
        socket = rt.socket,
        pause = PAUSE_IMAGE
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
            stop_grace_secs: None,
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
    fn containerd_config_is_v3_cri_with_runc_cgroupfs() {
        let rt = RuntimeSection {
            socket: "/run/containerd/containerd.sock".into(),
            ..Default::default()
        };
        let toml_str = containerd_config_toml(&rt);
        // Parses as valid TOML.
        let parsed: toml::Value = toml::from_str(&toml_str).expect("valid TOML");
        assert_eq!(parsed.get("version").and_then(|v| v.as_integer()), Some(3));
        // The 2.x CRI runtime plugin key is present.
        assert!(
            toml_str.contains("io.containerd.cri.v1.runtime"),
            "{toml_str}"
        );
        // runc runtime via the v2 shim, cgroupfs driver, explicit runc path.
        assert!(
            toml_str.contains("runtime_type = \"io.containerd.runc.v2\""),
            "{toml_str}"
        );
        assert!(toml_str.contains("SystemdCgroup = false"), "{toml_str}");
        assert!(
            toml_str.contains("BinaryName = \"/boot/bin/runc\""),
            "{toml_str}"
        );
        // root/state dirs set.
        assert!(
            toml_str.contains("root = \"/var/lib/containerd\""),
            "{toml_str}"
        );
        assert!(
            toml_str.contains("state = \"/run/containerd\""),
            "{toml_str}"
        );
        // socket threaded through.
        assert!(
            toml_str.contains("address = \"/run/containerd/containerd.sock\""),
            "{toml_str}"
        );
    }

    #[test]
    fn config_sets_sandbox_image() {
        let toml_str = containerd_config_toml(&RuntimeSection::default());
        assert!(toml_str.contains(&format!("sandbox_image = \"{PAUSE_IMAGE}\"")), "{toml_str}");
        // still valid TOML.
        toml::from_str::<toml::Value>(&toml_str).expect("valid TOML");
    }

    #[test]
    fn ctr_import_argv_is_k8s_namespaced() {
        let argv = ctr_import_args("/run/containerd/containerd.sock", "/boot/images/busybox.tar");
        assert_eq!(
            argv,
            vec![
                "--address", "/run/containerd/containerd.sock",
                "-n", "k8s.io",
                "images", "import", "/boot/images/busybox.tar",
            ]
        );
    }

    #[test]
    fn rejects_reserved_id() {
        let bad = vec![ServiceConfig {
            id: "containerd".into(),
            command: vec!["/bin/x".into()],
            depends_on: vec![],
            restart: Default::default(),
            stop_grace_secs: None,
        }];
        assert!(validate_services(&bad).is_err());
        assert!(validate_services(&[]).is_ok());
    }
}
