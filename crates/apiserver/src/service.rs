//! `MachineService` gRPC implementation over the COSI store.

use machined_runtime_core::State;
use tonic::{Request, Response, Status};

use crate::pb::machine_service_server::MachineService;
use crate::pb::{Empty, ListResourcesRequest, ListResourcesResponse, VersionResponse};

/// gRPC service backed by the resource store.
pub struct Machine {
    state: State,
    version: String,
}

impl Machine {
    pub fn new(state: State, version: impl Into<String>) -> Self {
        Self {
            state,
            version: version.into(),
        }
    }
}

#[tonic::async_trait]
impl MachineService for Machine {
    async fn version(&self, _req: Request<Empty>) -> Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: self.version.clone(),
        }))
    }

    async fn list_resources(
        &self,
        req: Request<ListResourcesRequest>,
    ) -> Result<Response<ListResourcesResponse>, Status> {
        // Filled in Task 3; placeholder so the service compiles in Task 2.
        let _ = (&self.state, req);
        Ok(Response::new(ListResourcesResponse {
            entries: Vec::new(),
        }))
    }
}
