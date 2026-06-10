//! Writes `/etc/resolv.conf` from the desired `ResolverSpec`.

use async_trait::async_trait;
use machined_resources::{Resource, ResourceType};
use machined_runtime_core::{Controller, Input, InputKind, Output, ReconcileCtx};

use super::{ctl, NS};

/// Controller writing resolv.conf. The path is injectable for tests.
pub struct ResolverController {
    path: std::path::PathBuf,
}

impl ResolverController {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Default production path.
    pub fn at_etc() -> Self {
        Self::new("/etc/resolv.conf")
    }
}

#[async_trait]
impl Controller for ResolverController {
    fn name(&self) -> &str {
        "resolver"
    }

    fn inputs(&self) -> Vec<Input> {
        vec![Input {
            namespace: NS.to_string(),
            typ: ResourceType::ResolverSpec,
            kind: InputKind::Strong,
        }]
    }

    fn outputs(&self) -> Vec<Output> {
        Vec::new()
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        for obj in ctx.state.list(NS, ResourceType::ResolverSpec) {
            if let Resource::ResolverSpec(s) = &obj.spec {
                let mut body = String::new();
                for sd in &s.search {
                    body.push_str(&format!("search {sd}\n"));
                }
                for ns in &s.nameservers {
                    body.push_str(&format!("nameserver {ns}\n"));
                }
                // Atomic write: temp file + rename.
                let tmp = self.path.with_extension("tmp");
                std::fs::write(&tmp, &body).map_err(ctl)?;
                std::fs::rename(&tmp, &self.path).map_err(ctl)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{ResolverSpec, ResourceObject};
    use machined_runtime_core::{ReconcileCtx, State};
    use std::net::{IpAddr, Ipv4Addr};

    #[tokio::test]
    async fn writes_resolv_conf() {
        let dir = std::env::temp_dir().join(format!("mnd-resolv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("resolv.conf");

        let state = State::new();
        state
            .create(ResourceObject::new(
                NS,
                "resolver",
                Resource::ResolverSpec(ResolverSpec {
                    nameservers: vec![IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))],
                    search: vec!["example.com".into()],
                }),
            ))
            .unwrap();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = ResolverController::new(&path);
        c.reconcile(&ctx).await.unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("search example.com"));
        assert!(written.contains("nameserver 1.1.1.1"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
