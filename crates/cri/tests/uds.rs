//! Root-free: a fake CRI server on a UnixListener, probed by the real
//! GrpcCriClient — proves the UDS connector + trimmed wire format end-to-end.

use machined_cri::pb::runtime_service_server::{RuntimeService, RuntimeServiceServer};
use machined_cri::pb::{
    RuntimeCondition, RuntimeStatus, StatusRequest, StatusResponse, VersionRequest, VersionResponse,
};
use machined_cri::{CriClient, GrpcCriClient};
use tonic::{Request, Response, Status};

struct FakeCriServer {
    ready: bool,
}

#[tonic::async_trait]
impl RuntimeService for FakeCriServer {
    async fn version(
        &self,
        _r: Request<VersionRequest>,
    ) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: "0.1.0".into(),
            runtime_name: "containerd".into(),
            runtime_version: "2.0.0".into(),
            runtime_api_version: "v1".into(),
        }))
    }

    async fn status(&self, _r: Request<StatusRequest>) -> Result<Response<StatusResponse>, Status> {
        Ok(Response::new(StatusResponse {
            status: Some(RuntimeStatus {
                conditions: vec![
                    RuntimeCondition {
                        r#type: "RuntimeReady".into(),
                        status: self.ready,
                        reason: String::new(),
                        message: String::new(),
                    },
                    RuntimeCondition {
                        r#type: "NetworkReady".into(),
                        status: false, // must NOT gate readiness
                        reason: String::new(),
                        message: String::new(),
                    },
                ],
            }),
            info: Default::default(),
        }))
    }
}

async fn spawn_server(ready: bool) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mnd-cri-{}-{}", std::process::id(), ready));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("cri.sock");
    std::fs::remove_file(&sock).ok();
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(RuntimeServiceServer::new(FakeCriServer { ready }))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    sock
}

#[tokio::test]
async fn probes_a_cri_server_over_uds() {
    let sock = spawn_server(true).await;
    let client = GrpcCriClient::new(&sock);
    let v = client.version().await.unwrap();
    assert_eq!(v.runtime_name, "containerd");
    assert_eq!(v.runtime_version, "2.0.0");
    assert!(
        client.ready().await.unwrap(),
        "RuntimeReady=true must be ready"
    );
}

#[tokio::test]
async fn not_ready_when_runtime_condition_false() {
    let sock = spawn_server(false).await;
    let client = GrpcCriClient::new(&sock);
    assert!(!client.ready().await.unwrap());
}

#[tokio::test]
async fn missing_socket_is_a_connect_error() {
    let client = GrpcCriClient::new("/no/such/cri.sock");
    assert!(client.version().await.is_err());
}

#[tokio::test]
#[ignore = "requires a running containerd at /run/containerd/containerd.sock"]
async fn probes_real_containerd() {
    let client = GrpcCriClient::new("/run/containerd/containerd.sock");
    let v = client.version().await.unwrap();
    assert!(!v.runtime_name.is_empty());
}
