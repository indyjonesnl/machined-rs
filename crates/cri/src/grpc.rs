//! Real CRI client: tonic gRPC over the containerd unix socket.

use std::path::PathBuf;

use async_trait::async_trait;
use hyper_util::rt::TokioIo;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use crate::pb::runtime_service_client::RuntimeServiceClient;
use crate::pb::{StatusRequest, VersionRequest};
use crate::{CriClient, CriError, Result, RuntimeVersion};

/// CRI over a unix socket. Connects per-probe (the socket may appear only after
/// containerd starts; a connect failure is the transient-unreachable case).
pub struct GrpcCriClient {
    socket: PathBuf,
}

impl GrpcCriClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    async fn connect(&self) -> Result<RuntimeServiceClient<Channel>> {
        let path = self.socket.clone();
        // The URI is ignored; the connector dials the unix socket.
        let channel = Endpoint::try_from("http://[::]:50051")
            .map_err(|e| CriError::Connect(e.to_string()))?
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await
            .map_err(|e| CriError::Connect(e.to_string()))?;
        Ok(RuntimeServiceClient::new(channel))
    }
}

#[async_trait]
impl CriClient for GrpcCriClient {
    async fn version(&self) -> Result<RuntimeVersion> {
        let mut client = self.connect().await?;
        let resp = client
            .version(VersionRequest::default())
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(RuntimeVersion {
            runtime_name: resp.runtime_name,
            runtime_version: resp.runtime_version,
        })
    }

    async fn ready(&self) -> Result<bool> {
        let mut client = self.connect().await?;
        let resp = client
            .status(StatusRequest::default())
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp
            .status
            .map(|s| {
                s.conditions
                    .iter()
                    .any(|c| c.r#type == "RuntimeReady" && c.status)
            })
            .unwrap_or(false))
    }
}
