//! Real CRI client: tonic gRPC over the containerd unix socket.

use std::path::PathBuf;

use async_trait::async_trait;
use hyper_util::rt::TokioIo;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use crate::pb::image_service_client::ImageServiceClient;
use crate::pb::runtime_service_client::RuntimeServiceClient;
use crate::pb::{
    ImageSpec, ImageStatusRequest, LinuxPodSandboxConfig, LinuxSandboxSecurityContext,
    ListPodSandboxRequest, NamespaceMode, NamespaceOption, PodSandboxConfig, PodSandboxFilter,
    PodSandboxMetadata, PullImageRequest, RunPodSandboxRequest, StatusRequest, VersionRequest,
};
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

    async fn connect_image(&self) -> Result<ImageServiceClient<Channel>> {
        let path = self.socket.clone();
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
        Ok(ImageServiceClient::new(channel))
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

    async fn image_present(&self, image: &str) -> Result<bool> {
        let mut client = self.connect_image().await?;
        let resp = client
            .image_status(ImageStatusRequest {
                image: Some(ImageSpec {
                    image: image.to_string(),
                }),
                verbose: false,
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp.image.is_some())
    }

    async fn pull_image(&self, image: &str) -> Result<()> {
        let mut client = self.connect_image().await?;
        client
            .pull_image(PullImageRequest {
                image: Some(ImageSpec {
                    image: image.to_string(),
                }),
                sandbox_config: None,
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?;
        Ok(())
    }

    async fn run_pod_sandbox(&self, pod: &crate::PodSpec) -> Result<String> {
        let mut client = self.connect().await?;
        let net = if pod.host_network {
            NamespaceMode::Node
        } else {
            NamespaceMode::Pod
        } as i32;
        let cfg = PodSandboxConfig {
            metadata: Some(PodSandboxMetadata {
                name: pod.name.clone(),
                uid: pod.uid.clone(),
                namespace: "default".into(),
                attempt: 0,
            }),
            hostname: pod.name.clone(),
            log_directory: String::new(),
            labels: std::collections::HashMap::from([(
                "io.machined.pod".to_string(),
                pod.name.clone(),
            )]),
            linux: Some(LinuxPodSandboxConfig {
                cgroup_parent: String::new(),
                security_context: Some(LinuxSandboxSecurityContext {
                    namespace_options: Some(NamespaceOption {
                        network: net,
                        pid: 0,
                        ipc: 0,
                    }),
                }),
            }),
        };
        let resp = client
            .run_pod_sandbox(RunPodSandboxRequest {
                config: Some(cfg),
                runtime_handler: String::new(),
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp.pod_sandbox_id)
    }

    async fn find_sandbox(&self, name: &str) -> Result<Option<String>> {
        let mut client = self.connect().await?;
        let resp = client
            .list_pod_sandbox(ListPodSandboxRequest {
                filter: Some(PodSandboxFilter {
                    id: String::new(),
                    state: None,
                    label_selector: std::collections::HashMap::from([(
                        "io.machined.pod".to_string(),
                        name.to_string(),
                    )]),
                }),
            })
            .await
            .map_err(|e| CriError::Rpc(e.to_string()))?
            .into_inner();
        Ok(resp.items.into_iter().next().map(|s| s.id))
    }
}
