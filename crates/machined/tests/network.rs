//! End-to-end: a network config, run through the controllers on the real
//! Runtime against a fake backend, configures the (simulated) kernel and
//! publishes status — no root required.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{
    InterfaceConfig, MachineConfig, MachineSection, NetworkSection, Provider, RouteConfig,
};
use machined_controllers::network::{
    AddressController, LinkController, NetworkConfigController, RouteController, NS,
};
use machined_netlink::{FakeNetworkBackend, NetworkBackend};
use machined_resources::{Key, ResourceType};
use machined_runtime_core::Runtime;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn config_drives_network_through_controllers() {
    let backend = Arc::new(
        FakeNetworkBackend::new()
            .with_link("eth0", 1500)
            .with_link("lo", 65536),
    );

    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: NetworkSection {
                interfaces: vec![InterfaceConfig {
                    name: "eth0".into(),
                    up: true,
                    mtu: Some(9000),
                    addresses: vec!["10.0.0.5/24".into()],
                    routes: vec![RouteConfig {
                        to: None,
                        via: "10.0.0.1".parse().unwrap(),
                        metric: None,
                    }],
                }],
                nameservers: vec![],
                search: vec![],
            },
            install: None,
            time: Default::default(),
            runtime: Default::default(),
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(NetworkConfigController::new(Provider::new(
        config,
    ))));
    runtime.register(Box::new(LinkController::new(backend.clone())));
    runtime.register(Box::new(AddressController::new(backend.clone())));
    runtime.register(Box::new(RouteController::new(backend.clone())));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    // Poll until the link is up + mtu applied + address present (the config
    // controller's initial reconcile creates specs, which wake the spec
    // controllers).
    let mut ok = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let links = backend.list_links().await.unwrap();
        let addrs = backend.list_addresses("eth0").await.unwrap();
        if links.first().map(|l| l.up && l.mtu == 9000) == Some(true)
            && addrs.iter().any(|a| a.to_string() == "10.0.0.5/24")
            && backend.routes().len() == 1
        {
            ok = true;
            break;
        }
    }
    assert!(
        ok,
        "network was not fully configured through the controllers"
    );

    // All three status resources published (the live-runtime status path).
    assert!(state
        .get(&Key::new(NS, ResourceType::LinkStatus, "eth0"))
        .is_ok());
    assert!(state
        .get(&Key::new(
            NS,
            ResourceType::AddressStatus,
            "eth0/10.0.0.5/24"
        ))
        .is_ok());
    assert!(state
        .get(&Key::new(
            NS,
            ResourceType::RouteStatus,
            "eth0/default/10.0.0.1"
        ))
        .is_ok());

    shutdown.cancel();
    let _ = handle.await;
}
