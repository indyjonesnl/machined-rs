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

/// What machined needs to start a pod sandbox. Vendor-neutral (no pb:: types).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodSpec {
    pub name: String,
    pub uid: String,
    /// true → the sandbox shares the node network namespace (no CNI).
    pub host_network: bool,
}

/// What machined needs to create a container in a sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub args: Vec<String>,
}

/// Vendor-neutral container state (maps CRI ContainerState).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Exited,
    Unknown,
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
    /// Create a pod sandbox; returns its id (CRI RunPodSandbox).
    async fn run_pod_sandbox(&self, pod: &PodSpec) -> Result<String>;
    /// Find a READY sandbox whose metadata name == `name`; the labelled id, if any.
    async fn find_sandbox(&self, name: &str) -> Result<Option<String>>;
    /// Create a container in a sandbox; returns its id (CRI CreateContainer).
    async fn create_container(&self, sandbox_id: &str, c: &ContainerSpec) -> Result<String>;
    /// Start a created container (CRI StartContainer).
    async fn start_container(&self, container_id: &str) -> Result<()>;
    /// Find a container by metadata name within a sandbox; its id, if any.
    async fn find_container(&self, sandbox_id: &str, name: &str) -> Result<Option<String>>;
    /// Read a container's current state (CRI ContainerStatus).
    async fn container_state(&self, container_id: &str) -> Result<ContainerState>;
}
