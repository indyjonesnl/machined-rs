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

pub use service::Machine;

/// Serve the management API over mutual TLS until the process exits.
pub async fn serve(
    addr: SocketAddr,
    state: State,
    version: impl Into<String>,
    pki: &NodePki,
) -> Result<(), tonic::transport::Error> {
    let svc = pb::machine_service_server::MachineServiceServer::new(Machine::new(state, version));
    let tls = server_tls(pki);
    Server::builder()
        .tls_config(tls)?
        .add_service(svc)
        .serve(addr)
        .await
}

/// Build the mutual-TLS config (server identity + required client CA). Filled in
/// Task 4; for Task 2 it is plaintext so the codegen/transport can be validated.
fn server_tls(_pki: &NodePki) -> tonic::transport::ServerTlsConfig {
    tonic::transport::ServerTlsConfig::new()
}
