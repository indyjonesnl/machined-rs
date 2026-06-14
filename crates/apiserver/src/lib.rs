//! machined management gRPC API server.

pub mod mapping;
pub mod service;

/// Generated protobuf types.
pub mod pb {
    tonic::include_proto!("machine");
}

use std::net::SocketAddr;

use machined_pki::NodePki;
use machined_runtime_core::State;
use tonic::transport::Server;

pub use service::{Machine, NodeAction};

/// Serve the management API over mutual TLS until the process exits.
pub async fn serve(
    addr: SocketAddr,
    state: State,
    version: impl Into<String>,
    image_id: impl Into<String>,
    pki: &NodePki,
    actions: tokio::sync::mpsc::Sender<NodeAction>,
) -> Result<(), tonic::transport::Error> {
    let svc = pb::machine_service_server::MachineServiceServer::new(Machine::new(
        state, version, image_id, actions,
    ));
    let tls = server_tls(pki);
    Server::builder()
        .tls_config(tls)?
        .add_service(svc)
        .serve(addr)
        .await
}

/// Serve the management API over mutual TLS until `signal` resolves.
pub async fn serve_with_shutdown(
    addr: SocketAddr,
    state: State,
    version: impl Into<String>,
    image_id: impl Into<String>,
    pki: &NodePki,
    actions: tokio::sync::mpsc::Sender<NodeAction>,
    signal: impl std::future::Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    let svc = pb::machine_service_server::MachineServiceServer::new(Machine::new(
        state, version, image_id, actions,
    ));
    let tls = server_tls(pki);
    Server::builder()
        .tls_config(tls)?
        .add_service(svc)
        .serve_with_shutdown(addr, signal)
        .await
}

/// Build the mutual-TLS config: the node's server identity plus the node CA as
/// the required client-certificate root (so only CA-signed clients connect).
fn server_tls(pki: &NodePki) -> tonic::transport::ServerTlsConfig {
    use tonic::transport::{Certificate, Identity, ServerTlsConfig};
    let (cert, key) = pki.server_identity();
    ServerTlsConfig::new()
        .identity(Identity::from_pem(cert, key))
        .client_ca_root(Certificate::from_pem(pki.ca_pem()))
}
