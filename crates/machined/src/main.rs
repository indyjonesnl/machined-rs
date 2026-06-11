//! machined — PID 1 / machine-management daemon entrypoint.

mod emergency;
mod pid1;

use std::path::PathBuf;
use std::sync::Arc;

use machined_common::init_logging;
use machined_config::{load::load_from_path, Provider};
use machined_controllers::block::DiskDiscoveryController;
use machined_controllers::network::{
    AddressController, HostnameController, LinkController, NetworkConfigController,
    ResolverController, RouteController,
};
use machined_platform::Platform;
use machined_runtime_core::Runtime;
use machined_sequencer::{boot_sequence, shutdown_sequence, SequencerCtx};
use machined_supervisor::ServiceManager;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

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

fn build_block_backend() -> Arc<dyn machined_block::BlockBackend> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_block::SysfsBlock::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_block::FakeBlockBackend::new())
    }
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

    runtime.register(Box::new(
        DiskDiscoveryController::new(build_block_backend()),
    ));

    let rt_token = shutdown.clone();
    let rt_handle = tokio::spawn(async move {
        if let Err(e) = runtime.run(rt_token).await {
            error!("runtime error: {e}");
        }
    });

    let ctx = SequencerCtx {
        state,
        platform: platform.clone(),
        provider,
        services: services.clone(),
    };

    // Boot.
    if let Err(e) = boot_sequence().run(&ctx).await {
        emergency::enter_emergency(&platform, &e, false);
        return Err(anyhow::anyhow!("boot failed: {e}"));
    }
    info!("boot complete; node up");

    // Wait for a termination signal.
    pid1::wait_for_termination().await;
    info!("shutting down");

    // Shutdown.
    if let Err(e) = shutdown_sequence().run(&ctx).await {
        error!("shutdown sequence error: {e}");
    }

    // Stop the runtime and join.
    shutdown.cancel();
    let _ = rt_handle.await;
    info!("machined stopped");
    Ok(())
}
