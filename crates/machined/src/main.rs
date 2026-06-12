//! machined — PID 1 / machine-management daemon entrypoint.

mod emergency;
mod pid1;

use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use machined_apiserver::NodeAction;
use machined_common::init_logging;
use machined_config::{load::load_from_path, Provider};
use machined_controllers::block::{
    DiskDiscoveryController, VolumeMountController, VolumeProvisionerController,
};
use machined_controllers::network::{
    AddressController, HostnameController, LinkController, NetworkConfigController,
    ResolverController, RouteController,
};
use machined_controllers::runtime::RuntimeHealthController;
use machined_controllers::time::TimeSyncController;
use machined_pki::NodePki;
use machined_platform::Platform;
use machined_runtime_core::Runtime;
use machined_sequencer::{boot_sequence, shutdown_sequence, SequencerCtx};
use machined_supervisor::ServiceManager;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const DEFAULT_CONFIG_PATH: &str = "/etc/machined/config.yaml";

fn build_platform() -> Arc<dyn Platform> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_platform::LinuxPlatform::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_platform::FakePlatform::new())
    }
}

fn build_network_backend() -> Arc<dyn machined_netlink::NetworkBackend> {
    #[cfg(target_os = "linux")]
    {
        match machined_netlink::RtNetlink::new() {
            Ok(b) => Arc::new(b),
            Err(e) => {
                error!("failed to open netlink ({e}); using inert fake backend");
                Arc::new(machined_netlink::FakeNetworkBackend::new())
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_netlink::FakeNetworkBackend::new())
    }
}

fn build_block_provisioner() -> Arc<dyn machined_block::BlockProvisioner> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_block::SysfsBlock::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_block::FakeBlockBackend::new())
    }
}

fn build_block_backend_for_discovery() -> Arc<dyn machined_block::BlockBackend> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_block::SysfsBlock::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_block::FakeBlockBackend::new())
    }
}

fn build_cri(socket: &str) -> Arc<dyn machined_cri::CriClient> {
    Arc::new(machined_cri::GrpcCriClient::new(socket))
}

fn build_time_sync() -> Arc<dyn machined_time::TimeSync> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_time::SntpTime::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_time::FakeTimeSync::new())
    }
}

/// Default service readiness, plus: the built-in containerd service is ready
/// only once the CRI probe reports the runtime ready (RuntimeStatus.ready).
struct RuntimeReadiness;

impl machined_supervisor::ReadinessCheck for RuntimeReadiness {
    fn is_ready(&self, state: &machined_runtime_core::State, dep_id: &str) -> bool {
        use machined_resources::{Key, Resource, ResourceType};
        let base = machined_supervisor::DefaultReadiness.is_ready(state, dep_id);
        if dep_id != machined_config::RUNTIME_SERVICE_ID {
            return base;
        }
        // base may also be true via Finished (a stopped containerd) with a
        // momentarily-stale ready=true; harmless — containerd is restart:Always
        // and the next CRI probe flips ready=false within one tick.
        let cri_ready = matches!(
            state
                .get(&Key::new(NS_RUNTIME, ResourceType::RuntimeStatus, "containerd"))
                .map(|o| o.spec),
            Ok(Resource::RuntimeStatus(r)) if r.ready
        );
        base && cri_ready
    }
}

const NS_RUNTIME: &str = "runtime";

/// Write a machinectl client bundle (ca + a fresh client cert) into `dir`.
fn write_client_bundle(dir: &Path, pki: &NodePki) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let client = pki.issue_client("machinectl")?;
    std::fs::write(dir.join("ca.pem"), pki.ca_pem())?;
    std::fs::write(dir.join("client.pem"), &client.cert_pem)?;
    std::fs::write(dir.join("client.key"), &client.key_pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.join("client.key"),
            std::fs::Permissions::from_mode(0o600),
        )?;
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    init_logging();

    // Multi-call dispatch: argv[1] selects a subcommand; default is the daemon.
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("version") => {
            println!("machined {}", env!("CARGO_PKG_VERSION"));
        }
        Some("daemon") | None => {
            if let Err(e) = run_daemon().await {
                error!("daemon exited with error: {e}");
                std::process::exit(1);
            }
        }
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            std::process::exit(2);
        }
    }
}

/// What the daemon does after the graceful shutdown sequence.
enum FinalAction {
    Stop,
    Reboot,
    Poweroff,
    Reset,
}

/// A final syscall (reboot/poweroff) failed: PID1 must never exit. Enter the
/// emergency state and park forever.
async fn park_after_failed_final(
    platform: &Arc<dyn Platform>,
    // `+ Sync` so the future is Send (it is held across .await in spawned tasks).
    err: &(dyn std::fmt::Display + Sync),
) {
    emergency::enter_emergency(platform, err, false);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

/// Reset: re-format STATE + EPHEMERAL in place (labels preserved) so the next
/// boot reprovisions fresh volumes. Best-effort — failures log and the reset
/// still proceeds to reboot.
async fn perform_reset(
    state: &machined_runtime_core::State,
    prov: &dyn machined_block::BlockProvisioner,
) {
    use machined_block::FsType;
    use machined_controllers::block::NS as BLOCK_NS;
    use machined_resources::{Key, Resource, ResourceType};
    // per-label fallback when the recorded fs is empty/unknown (corrupt fs —
    // exactly the volume reset most needs to wipe): the fixed layout's type.
    fn fallback_fs(label: &str) -> Option<machined_block::FsType> {
        match label {
            "STATE" | "EPHEMERAL" => Some(machined_block::FsType::Ext4),
            _ => None,
        }
    }
    for label in ["STATE", "EPHEMERAL"] {
        let key = Key::new(BLOCK_NS, ResourceType::VolumeStatus, label);
        let vol = match state.get(&key).map(|o| o.spec) {
            Ok(Resource::VolumeStatus(v)) => v,
            _ => {
                warn!("reset: no VolumeStatus for {label}; skipping");
                continue;
            }
        };
        let Some(fs) = FsType::from_str_name(&vol.fs).or_else(|| fallback_fs(label)) else {
            warn!("reset: unknown fs '{}' for {label}; skipping", vol.fs);
            continue;
        };
        info!("reset: formatting {} ({}, {label})", vol.device, vol.fs);
        if let Err(e) = prov.format(&vol.device, fs, label).await {
            error!("reset: format {} failed: {e}", vol.device);
        }
    }
}

async fn run_daemon() -> anyhow::Result<()> {
    info!("machined starting (pid {})", std::process::id());
    let platform = build_platform();
    let shutdown = CancellationToken::new();

    // PID-1 duties.
    pid1::spawn_reaper(shutdown.clone());

    // Load config first so the controllers can be built from it (fall back to
    // an empty config if the file is absent, so a bare boot still comes up).
    let config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let (config, _raw) = match load_from_path(&config_path) {
        Ok(v) => v,
        Err(e) => {
            info!(
                "no config at {} ({e}); booting with defaults",
                config_path.display()
            );
            (Default::default(), String::new())
        }
    };
    let provider = Provider::new(config);

    // Build the shared runtime + service manager, registering the network
    // controllers so the node configures its network from config.
    let mut runtime = Runtime::new();
    let state = runtime.state();
    let services = Arc::new(Mutex::new(ServiceManager::new(state.clone())));

    let net_backend = build_network_backend();
    runtime.register(Box::new(NetworkConfigController::new(provider.clone())));
    runtime.register(Box::new(LinkController::new(net_backend.clone())));
    runtime.register(Box::new(AddressController::new(net_backend.clone())));
    runtime.register(Box::new(RouteController::new(net_backend.clone())));
    runtime.register(Box::new(HostnameController::new(platform.clone())));
    runtime.register(Box::new(ResolverController::at_etc()));

    let block = build_block_provisioner();
    let block_for_reset = block.clone();
    // BlockProvisioner is a supertrait of BlockBackend; a fresh trait object for
    // discovery is built from the same concrete type.
    runtime.register(Box::new(DiskDiscoveryController::new(
        build_block_backend_for_discovery(),
    )));
    runtime.register(Box::new(VolumeProvisionerController::new(
        block,
        provider.clone(),
    )));
    runtime.register(Box::new(VolumeMountController::new(platform.clone())));

    runtime.register(Box::new(TimeSyncController::new(
        build_time_sync(),
        provider.clone(),
    )));

    runtime.register(Box::new(RuntimeHealthController::new(
        build_cri(&provider.runtime().socket),
        provider.clone(),
    )));

    let rt_token = shutdown.clone();
    let rt_handle = tokio::spawn(async move {
        if let Err(e) = runtime.run(rt_token).await {
            error!("runtime error: {e}");
        }
    });

    // Action channel: API handlers enqueue; the main loop below consumes one.
    let (api_action_tx, mut api_action_rx) = tokio::sync::mpsc::channel::<NodeAction>(1);

    // Management API (M3a): node PKI + mTLS gRPC server, sharing the COSI store.
    let pki_dir = std::path::PathBuf::from("/system/state/pki");
    let mut api_handle: Option<tokio::task::JoinHandle<()>> = None;
    match NodePki::load_or_generate(&pki_dir, "node", &["127.0.0.1".into(), "localhost".into()]) {
        Ok(pki) => {
            if let Err(e) = write_client_bundle(&pki_dir.join("machinectl"), &pki) {
                error!("writing machinectl bundle: {e}");
            }
            let api_addr: SocketAddr = "127.0.0.1:50000".parse().unwrap();
            let api_state = state.clone();
            let api_action_tx = api_action_tx.clone();
            let api_token = shutdown.clone();
            api_handle = Some(tokio::spawn(async move {
                if let Err(e) = machined_apiserver::serve_with_shutdown(
                    api_addr,
                    api_state,
                    env!("CARGO_PKG_VERSION"),
                    &pki,
                    api_action_tx,
                    {
                        let t = api_token;
                        async move { t.cancelled().await }
                    },
                )
                .await
                {
                    error!("apiserver exited: {e}");
                }
            }));
            info!("management API listening on {api_addr}");
        }
        Err(e) => error!("PKI init failed; management API disabled: {e}"),
    }

    let state_for_reset = state.clone();
    let ctx = SequencerCtx {
        state,
        platform: platform.clone(),
        provider,
        services: services.clone(),
        readiness: Arc::new(RuntimeReadiness),
    };

    // Boot.
    if let Err(e) = boot_sequence().run(&ctx).await {
        emergency::enter_emergency(&platform, &e, false);
        return Err(anyhow::anyhow!("boot failed: {e}"));
    }
    info!("boot complete; node up");

    // Wait for an OS termination signal OR an API-requested action.
    let final_action = tokio::select! {
        _ = pid1::wait_for_termination() => FinalAction::Stop,
        a = api_action_rx.recv() => match a {
            Some(NodeAction::Reboot) => FinalAction::Reboot,
            Some(NodeAction::Shutdown) => FinalAction::Poweroff,
            Some(NodeAction::Reset) => FinalAction::Reset,
            None => FinalAction::Stop,
        },
    };
    info!("shutting down");

    // Stop the controller runtime FIRST: no controller may act (e.g. re-mount)
    // while services stop, volumes unmount, or a reset formats partitions.
    shutdown.cancel();
    let _ = rt_handle.await;
    if let Some(mut h) = api_handle {
        if tokio::time::timeout(std::time::Duration::from_secs(5), &mut h)
            .await
            .is_err()
        {
            warn!("api server did not shut down in time; aborting");
            h.abort();
            let _ = h.await;
        }
    }

    // Graceful stop + disk teardown.
    if let Err(e) = shutdown_sequence().run(&ctx).await {
        error!("shutdown sequence error: {e}");
    }
    info!("machined stopped");

    match final_action {
        FinalAction::Stop => {}
        FinalAction::Reboot => {
            info!("rebooting");
            if let Err(e) = platform.reboot() {
                error!("reboot failed: {e}");
                park_after_failed_final(&platform, &e).await;
            }
        }
        FinalAction::Poweroff => {
            info!("powering off");
            if let Err(e) = platform.poweroff() {
                error!("poweroff failed: {e}");
                park_after_failed_final(&platform, &e).await;
            }
        }
        FinalAction::Reset => {
            info!("resetting: wiping STATE + EPHEMERAL, then rebooting");
            perform_reset(&state_for_reset, block_for_reset.as_ref()).await;
            if let Err(e) = platform.reboot() {
                error!("reboot failed: {e}");
                park_after_failed_final(&platform, &e).await;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{
        Resource, ResourceObject, RuntimeStatus, ServiceState, ServiceStatusSpec,
    };
    use machined_runtime_core::State;
    use machined_supervisor::ReadinessCheck;

    fn svc_running(state: &State, id: &str) {
        let _ = state.create(ResourceObject::new(
            "runtime",
            id,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: id.into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        ));
    }

    #[test]
    fn containerd_needs_cri_ready_too() {
        let state = State::new();
        svc_running(&state, "containerd");
        // Process running but CRI not ready → NOT ready.
        assert!(!RuntimeReadiness.is_ready(&state, "containerd"));

        let _ = state.create(ResourceObject::new(
            "runtime",
            "containerd",
            Resource::RuntimeStatus(RuntimeStatus {
                ready: true,
                name: "containerd".into(),
                version: "2".into(),
            }),
        ));
        assert!(RuntimeReadiness.is_ready(&state, "containerd"));
    }

    #[test]
    fn other_services_use_default_rule_only() {
        let state = State::new();
        svc_running(&state, "payload");
        // No RuntimeStatus anywhere — non-containerd ids don't need it.
        assert!(RuntimeReadiness.is_ready(&state, "payload"));
    }

    #[tokio::test]
    async fn failed_final_parks_forever() {
        let platform: Arc<dyn Platform> = Arc::new(machined_platform::FakePlatform::new());
        let parked = tokio::spawn(async move {
            park_after_failed_final(&platform, &"reboot failed (test)").await;
        });
        // It must NOT return.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), parked)
                .await
                .is_err(),
            "park must never complete"
        );
    }

    #[tokio::test]
    async fn reset_formats_exactly_state_and_ephemeral() {
        use machined_resources::{VolumePhase, VolumeStatus};

        let state = State::new();
        for (label, dev, fs) in [
            ("EFI", "/dev/vda1", "vfat"),
            ("STATE", "/dev/vda2", "ext4"),
            ("EPHEMERAL", "/dev/vda3", "ext4"),
        ] {
            let _ = state.create(ResourceObject::new(
                "block",
                label,
                Resource::VolumeStatus(VolumeStatus {
                    name: label.into(),
                    device: dev.into(),
                    fs: fs.into(),
                    label: label.into(),
                    phase: VolumePhase::Provisioned,
                }),
            ));
        }
        let fake = machined_block::FakeBlockBackend::new();
        perform_reset(&state, &fake).await;

        let formats = fake.formats();
        assert_eq!(formats.len(), 2, "exactly STATE + EPHEMERAL");
        assert!(formats.iter().any(|f| f.0 == "/dev/vda2"));
        assert!(formats.iter().any(|f| f.0 == "/dev/vda3"));
        assert!(
            !formats.iter().any(|f| f.0 == "/dev/vda1"),
            "EFI must NEVER be formatted by reset"
        );
        // No wipes / re-partitioning.
        assert!(fake.wipes().is_empty());
        assert!(fake.creates().is_empty());
    }

    #[tokio::test]
    async fn reset_formats_state_with_empty_fs_via_fallback() {
        use machined_resources::{VolumePhase, VolumeStatus};

        let state = State::new();
        // Empty fs string (e.g. corrupt/unprobeable filesystem) — exactly the
        // volume a reset most needs to wipe; the fixed-layout fallback applies.
        let _ = state.create(ResourceObject::new(
            "block",
            "STATE",
            Resource::VolumeStatus(VolumeStatus {
                name: "STATE".into(),
                device: "/dev/vda2".into(),
                fs: "".into(),
                label: "STATE".into(),
                phase: VolumePhase::Provisioned,
            }),
        ));
        let fake = machined_block::FakeBlockBackend::new();
        perform_reset(&state, &fake).await;

        let formats = fake.formats();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].0, "/dev/vda2");
        assert_eq!(formats[0].1, machined_block::FsType::Ext4);
        assert_eq!(formats[0].2, "STATE");
        // Still never wipes / re-partitions.
        assert!(fake.wipes().is_empty());
        assert!(fake.creates().is_empty());
    }

    #[tokio::test]
    async fn reset_without_volumes_degrades_to_noop() {
        let state = State::new();
        let fake = machined_block::FakeBlockBackend::new();
        perform_reset(&state, &fake).await;
        assert!(fake.formats().is_empty());
    }
}
