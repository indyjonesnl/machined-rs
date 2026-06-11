//! End-to-end: run the built `machinectl` binary against a real mTLS apiserver.
//! Root-free (loopback TLS). Uses the cargo-provided binary path.

use std::time::Duration;

use machined_pki::NodePki;
use machined_resources::{Resource, ResourceObject, ServiceState, ServiceStatusSpec};
use machined_runtime_core::State;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

#[tokio::test]
async fn machinectl_queries_a_real_server() {
    // PKI + a machinectl bundle on disk.
    let root = std::env::temp_dir().join(format!("mnd-mctl-{}", std::process::id()));
    let pki_dir = root.join("pki");
    let pki = NodePki::load_or_generate(&pki_dir, "node", &["127.0.0.1".into()]).unwrap();
    let bundle = root.join("bundle");
    std::fs::create_dir_all(&bundle).unwrap();
    let client = pki.issue_client("test").unwrap();
    std::fs::write(bundle.join("ca.pem"), pki.ca_pem()).unwrap();
    std::fs::write(bundle.join("client.pem"), &client.cert_pem).unwrap();
    std::fs::write(bundle.join("client.key"), &client.key_pem).unwrap();

    // A store with one seeded ServiceStatus.
    let state = State::new();
    state
        .create(ResourceObject::new(
            "runtime",
            "etcd",
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: "etcd".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: "ok".into(),
            }),
        ))
        .unwrap();

    // mTLS server on an ephemeral loopback port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let (scert, skey) = pki.server_identity();
    let ca_pem = pki.ca_pem();
    tokio::spawn(async move {
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(scert, skey))
            .client_ca_root(Certificate::from_pem(ca_pem));
        let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
            machined_apiserver::Machine::new(state, "1.2.3"),
        );
        Server::builder()
            .tls_config(tls)
            .unwrap()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(120)).await;

    let endpoint = format!("https://{addr}");
    let bin = env!("CARGO_BIN_EXE_machinectl");

    // `version`
    let out = tokio::process::Command::new(bin)
        .args([
            "--bundle",
            bundle.to_str().unwrap(),
            "--endpoint",
            &endpoint,
            "version",
        ])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "version failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1.2.3"), "version stdout: {stdout}");

    // `get ServiceStatus`
    let out2 = tokio::process::Command::new(bin)
        .args([
            "--bundle",
            bundle.to_str().unwrap(),
            "--endpoint",
            &endpoint,
            "get",
            "ServiceStatus",
            "--namespace",
            "runtime",
        ])
        .output()
        .await
        .unwrap();
    assert!(out2.status.success(), "get failed: {:?}", out2);
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("etcd"), "get stdout: {stdout2}");

    std::fs::remove_dir_all(&root).ok();
}
