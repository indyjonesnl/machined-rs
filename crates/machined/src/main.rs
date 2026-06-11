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
            None => FinalAction::Stop,
        },
    };
    info!("shutting down");

    // Shutdown.
    if let Err(e) = shutdown_sequence().run(&ctx).await {
        error!("shutdown sequence error: {e}");
    }

    // Stop the runtime and join.
    shutdown.cancel();
    let _ = rt_handle.await;
    if let Some(h) = api_handle {
        if tokio::time::timeout(std::time::Duration::from_secs(5), h)
            .await
            .is_err()
        {
            warn!("api server did not shut down in time");
        }
    }
    info!("machined stopped");

    match final_action {
        FinalAction::Stop => {}
        FinalAction::Reboot => {
            info!("rebooting");
            if let Err(e) = platform.reboot() {
                error!("reboot failed: {e}");
            }
        }
        FinalAction::Poweroff => {
            info!("powering off");
            if let Err(e) = platform.poweroff() {
                error!("poweroff failed: {e}");
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
}
