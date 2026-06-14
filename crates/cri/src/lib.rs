//! Minimal CRI client: just enough to know the container runtime is healthy.

pub mod fake;
pub mod grpc;

/// Generated (trimmed) CRI protobuf types.
pub mod pb {
    tonic::include_proto!("runtime.v1");
}

use async_trait::async_trait;

pub use fake::FakeCriClient;
pub use grpc::GrpcCriClient;

#[derive(thiserror::Error, Debug)]
pub enum CriError {
    #[error("cri connect: {0}")]
    Connect(String),
    #[error("cri rpc: {0}")]
    Rpc(String),
}

pub type Result<T> = std::result::Result<T, CriError>;

/// Identity of the running container runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeVersion {
    pub runtime_name: String,
    pub runtime_version: String,
}

/// Health-probe view of a CRI runtime.
#[async_trait]
pub trait CriClient: Send + Sync {
    /// RuntimeService.Version.
    async fn version(&self) -> Result<RuntimeVersion>;
    /// True iff the RuntimeReady condition is true (NetworkReady not required).
    async fn ready(&self) -> Result<bool>;
    /// True iff the image ref is present in the runtime's store (CRI ImageStatus).
    async fn image_present(&self, image: &str) -> Result<bool>;
    /// Pull an image by ref (CRI PullImage). Offline nodes pre-import instead;
    /// this is the fallback path when a registry is reachable.
    async fn pull_image(&self, image: &str) -> Result<()>;
}
