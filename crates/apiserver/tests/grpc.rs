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
        Machine::new(state, "9.9.9"),
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
        Machine::new(state, "9.9.9"),
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
