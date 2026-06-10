//! Privileged integration test: drives the real `RtNetlink` backend inside a
//! fresh network namespace against a dummy link. Ignored by default; run with:
//!   sudo -E cargo test -p machined-netlink --test netns -- --ignored
//! or in CI under `unshare -rn`. Requires CAP_NET_ADMIN.

#![cfg(target_os = "linux")]

use std::net::{IpAddr, Ipv4Addr};

use machined_netlink::{NetworkBackend, RtNetlink};
use machined_resources::AddrCidr;

#[tokio::test]
#[ignore = "requires root + network namespace (CAP_NET_ADMIN)"]
async fn dummy_link_up_addr_roundtrip() {
    // Create an isolated namespace so we never touch the host network.
    // (Run the whole test under `unshare -rn`; here we assume we are already in
    // a fresh netns and create a dummy link via `ip`.)
    let status = std::process::Command::new("ip")
        .args(["link", "add", "mnd0", "type", "dummy"])
        .status()
        .expect("run ip link add");
    assert!(status.success(), "failed to create dummy link");

    let be = RtNetlink::new().expect("open netlink");

    // Bring it up, set MTU.
    be.set_link_up("mnd0", true).await.unwrap();
    be.set_mtu("mnd0", 1400).await.unwrap();
    let links = be.list_links().await.unwrap();
    let mnd0 = links
        .iter()
        .find(|l| l.name == "mnd0")
        .expect("link present");
    assert!(mnd0.up, "link should be up");
    assert_eq!(mnd0.mtu, 1400);

    // Add and read back an address.
    let addr = AddrCidr::new(IpAddr::V4(Ipv4Addr::new(10, 9, 9, 1)), 24);
    be.add_address("mnd0", addr).await.unwrap();
    let addrs = be.list_addresses("mnd0").await.unwrap();
    assert!(
        addrs.contains(&addr),
        "address should be present: {addrs:?}"
    );

    // Delete it.
    be.del_address("mnd0", addr).await.unwrap();
    let addrs = be.list_addresses("mnd0").await.unwrap();
    assert!(!addrs.contains(&addr), "address should be removed");
}
