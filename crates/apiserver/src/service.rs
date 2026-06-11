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
}
