//! gRPC integration tests. Task 2 = plaintext Version; Task 4 adds mTLS.

use std::time::Duration;

use machined_apiserver::pb::machine_service_client::MachineServiceClient;
use machined_apiserver::pb::Empty;
use machined_apiserver::Machine;
use machined_runtime_core::State;
use tonic::transport::Server;

#[tokio::test]
async fn version_over_plaintext() {
    let state = State::new();
    let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
        Machine::new(state, "9.9.9", tokio::sync::mpsc::channel(1).0),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client = MachineServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let resp = client.version(Empty {}).await.unwrap().into_inner();
    assert_eq!(resp.version, "9.9.9");
}

#[tokio::test]
async fn lists_seeded_resources_over_plaintext() {
    use machined_apiserver::pb::ListResourcesRequest;
    use machined_resources::{Resource, ResourceObject, ServiceState, ServiceStatusSpec};

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

    let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
        Machine::new(state, "9.9.9", tokio::sync::mpsc::channel(1).0),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client = MachineServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let resp = client
        .list_resources(ListResourcesRequest {
            namespace: "runtime".into(),
            r#type: "ServiceStatus".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.entries.len(), 1);
    assert_eq!(resp.entries[0].id, "etcd");
    assert!(resp.entries[0]
        .fields
        .iter()
        .any(|f| f.key == "service_id" && f.value == "etcd"));
}

#[tokio::test]
async fn mtls_requires_a_valid_client_cert() {
    use machined_apiserver::pb::machine_service_client::MachineServiceClient;
    use machined_apiserver::pb::Empty;
    use machined_pki::NodePki;
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

    let dir = std::env::temp_dir().join(format!("mnd-api-tls-{}", std::process::id()));
    let pki = NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
    let ca = pki.ca_pem();
    let client_id = pki.issue_client("admin").unwrap();

    let state = State::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let pki_moved = pki;
    tokio::spawn(async move {
        let tls = {
            use tonic::transport::{Identity as Id, ServerTlsConfig};
            let (c, k) = pki_moved.server_identity();
            ServerTlsConfig::new()
                .identity(Id::from_pem(c, k))
                .client_ca_root(Certificate::from_pem(pki_moved.ca_pem()))
        };
        let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
            machined_apiserver::Machine::new(state, "9.9.9", tokio::sync::mpsc::channel(1).0),
        );
        Server::builder()
            .tls_config(tls)
            .unwrap()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Authorized client (CA-signed cert) succeeds.
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&ca))
        .identity(Identity::from_pem(&client_id.cert_pem, &client_id.key_pem))
        .domain_name("127.0.0.1");
    let channel = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = MachineServiceClient::new(channel);
    let resp = client.version(Empty {}).await.unwrap().into_inner();
    assert_eq!(resp.version, "9.9.9");

    // Unauthenticated client (no client identity) is rejected at the handshake.
    let tls_no_id = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&ca))
        .domain_name("127.0.0.1");
    let bad = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls_no_id)
        .unwrap()
        .connect()
        .await;
    assert!(
        bad.is_err() || {
            let mut c = MachineServiceClient::new(bad.unwrap());
            c.version(Empty {}).await.is_err()
        }
    );

    // Rogue client: a cert signed by a DIFFERENT (attacker) CA is rejected — it
    // does not chain to the node CA in client_ca_root. (The realistic attack.)
    let rogue_dir = std::env::temp_dir().join(format!("mnd-api-rogue-{}", std::process::id()));
    let rogue = NodePki::load_or_generate(&rogue_dir, "rogue", &["127.0.0.1".into()]).unwrap();
    let rogue_id = rogue.issue_client("attacker").unwrap();
    let tls_rogue = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&ca)) // still trust the real server
        .identity(Identity::from_pem(&rogue_id.cert_pem, &rogue_id.key_pem))
        .domain_name("127.0.0.1");
    let rogue_conn = Endpoint::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls_rogue)
        .unwrap()
        .connect()
        .await;
    assert!(
        rogue_conn.is_err() || {
            let mut c = MachineServiceClient::new(rogue_conn.unwrap());
            c.version(Empty {}).await.is_err()
        },
        "a client cert from a rogue CA must be rejected"
    );
    std::fs::remove_dir_all(&rogue_dir).ok();

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn reboot_and_shutdown_enqueue_actions() {
    use machined_apiserver::pb::machine_service_client::MachineServiceClient;
    use machined_apiserver::pb::Empty;
    use machined_apiserver::{Machine, NodeAction};

    let (tx, mut rx) = tokio::sync::mpsc::channel(3);
    let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
        Machine::new(State::new(), "9.9.9", tx),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client = MachineServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    client.reboot(Empty {}).await.unwrap();
    assert_eq!(rx.recv().await, Some(NodeAction::Reboot));
    client.shutdown(Empty {}).await.unwrap();
    assert_eq!(rx.recv().await, Some(NodeAction::Shutdown));
    client.reset(Empty {}).await.unwrap();
    assert_eq!(rx.recv().await, Some(NodeAction::Reset));
}

#[tokio::test]
async fn server_exits_on_shutdown_signal() {
    use tokio_util::sync::CancellationToken;

    let token = CancellationToken::new();
    let t2 = token.clone();
    let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
        Machine::new(State::new(), "9.9.9", tokio::sync::mpsc::channel(1).0),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let h = tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(incoming, async move { t2.cancelled().await })
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    token.cancel();
    let res = tokio::time::timeout(Duration::from_secs(3), h)
        .await
        .expect("server must exit on signal")
        .unwrap();
    assert!(res.is_ok());
}
