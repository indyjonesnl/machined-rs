//! `MachineService` gRPC implementation over the COSI store.

use machined_runtime_core::State;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

use crate::pb::machine_service_server::MachineService;
use crate::pb::{
    Empty, ListResourcesRequest, ListResourcesResponse, UpgradeRequest, VersionResponse,
};

/// A node lifecycle action requested via the API, handed to the daemon main loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeAction {
    Reboot,
    Shutdown,
    Reset,
    Upgrade { url: String, sha256: String },
}

/// gRPC service backed by the resource store.
pub struct Machine {
    state: State,
    version: String,
    image_id: String,
    actions: mpsc::Sender<NodeAction>,
}

impl Machine {
    pub fn new(
        state: State,
        version: impl Into<String>,
        image_id: impl Into<String>,
        actions: mpsc::Sender<NodeAction>,
    ) -> Self {
        Self {
            state,
            version: version.into(),
            image_id: image_id.into(),
            actions,
        }
    }
}

#[tonic::async_trait]
impl MachineService for Machine {
    async fn version(&self, _req: Request<Empty>) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: self.version.clone(),
            image_id: self.image_id.clone(),
        }))
    }

    async fn list_resources(
        &self,
        req: Request<ListResourcesRequest>,
    ) -> Result<Response<ListResourcesResponse>, Status> {
        let r = req.into_inner();
        let typ = crate::mapping::parse_resource_type(&r.r#type).ok_or_else(|| {
            Status::invalid_argument(format!("unknown resource type: {}", r.r#type))
        })?;
        let entries = self
            .state
            .list(&r.namespace, typ)
            .into_iter()
            .map(|obj| {
                let fields = crate::mapping::resource_to_fields(&obj.spec)
                    .into_iter()
                    .map(|(key, value)| crate::pb::KeyValue { key, value })
                    .collect();
                crate::pb::ResourceEntry {
                    id: obj.metadata.id,
                    fields,
                }
            })
            .collect();
        Ok(Response::new(ListResourcesResponse { entries }))
    }

    async fn reboot(&self, _req: Request<Empty>) -> Result<Response<Empty>, Status> {
        tracing::info!("reboot requested via API");
        self.actions
            .send(NodeAction::Reboot)
            .await
            .map_err(|_| Status::unavailable("daemon is shutting down"))?;
        Ok(Response::new(Empty {}))
    }

    async fn shutdown(&self, _req: Request<Empty>) -> Result<Response<Empty>, Status> {
        tracing::info!("shutdown requested via API");
        self.actions
            .send(NodeAction::Shutdown)
            .await
            .map_err(|_| Status::unavailable("daemon is shutting down"))?;
        Ok(Response::new(Empty {}))
    }

    async fn reset(&self, _req: Request<Empty>) -> Result<Response<Empty>, Status> {
        tracing::info!("reset requested via API");
        self.actions
            .send(NodeAction::Reset)
            .await
            .map_err(|_| Status::unavailable("daemon is shutting down"))?;
        Ok(Response::new(Empty {}))
    }

    async fn upgrade(&self, req: Request<UpgradeRequest>) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        tracing::info!(url = %r.url, "upgrade requested via API");
        self.actions
            .send(NodeAction::Upgrade {
                url: r.url,
                sha256: r.sha256,
            })
            .await
            .map_err(|_| Status::unavailable("daemon is shutting down"))?;
        Ok(Response::new(Empty {}))
    }
}
