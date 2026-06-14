//! In-memory CRI client for tests.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{CriClient, CriError, Result, RuntimeVersion};

#[derive(Default)]
struct FakeState {
    version: Option<RuntimeVersion>,
    ready: bool,
    calls: usize,
    images: std::collections::HashSet<String>,
    next_id: usize,
    sandboxes: Vec<(String, String)>, // (id, name)
    // (id, sandbox_id, name, running)
    containers: Vec<(String, String, String, bool)>,
}

#[derive(Default)]
pub struct FakeCriClient {
    state: Mutex<FakeState>,
}

impl FakeCriClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the runtime identity; absent → all calls error (unreachable).
    pub fn with_version(self, name: &str, version: &str) -> Self {
        self.state.lock().unwrap().version = Some(RuntimeVersion {
            runtime_name: name.into(),
            runtime_version: version.into(),
        });
        self
    }

    pub fn with_ready(self, ready: bool) -> Self {
        self.state.lock().unwrap().ready = ready;
        self
    }

    pub fn with_image(self, image: &str) -> Self {
        self.state.lock().unwrap().images.insert(image.to_string());
        self
    }

    pub fn calls(&self) -> usize {
        self.state.lock().unwrap().calls
    }

    pub fn sandbox_count(&self) -> usize {
        self.state.lock().unwrap().sandboxes.len()
    }
}

#[async_trait]
impl CriClient for FakeCriClient {
    async fn version(&self) -> Result<RuntimeVersion> {
        let mut s = self.state.lock().unwrap();
        s.calls += 1;
        s.version
            .clone()
            .ok_or_else(|| CriError::Connect("unreachable".into()))
    }

    async fn ready(&self) -> Result<bool> {
        let mut s = self.state.lock().unwrap();
        s.calls += 1;
        if s.version.is_none() {
            return Err(CriError::Connect("unreachable".into()));
        }
        Ok(s.ready)
    }

    async fn image_present(&self, image: &str) -> Result<bool> {
        Ok(self.state.lock().unwrap().images.contains(image))
    }

    async fn pull_image(&self, image: &str) -> Result<()> {
        self.state.lock().unwrap().images.insert(image.to_string());
        Ok(())
    }

    async fn run_pod_sandbox(&self, pod: &crate::PodSpec) -> Result<String> {
        let mut s = self.state.lock().unwrap();
        s.next_id += 1;
        let id = format!("sandbox-{}", s.next_id);
        s.sandboxes.push((id.clone(), pod.name.clone()));
        Ok(id)
    }

    async fn find_sandbox(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .sandboxes
            .iter()
            .find(|(_, n)| n == name)
            .map(|(id, _)| id.clone()))
    }

    async fn create_container(&self, sandbox_id: &str, c: &crate::ContainerSpec) -> Result<String> {
        let mut s = self.state.lock().unwrap();
        s.next_id += 1;
        let id = format!("ctr-{}", s.next_id);
        s.containers
            .push((id.clone(), sandbox_id.to_string(), c.name.clone(), false));
        Ok(id)
    }

    async fn start_container(&self, container_id: &str) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        if let Some(c) = s.containers.iter_mut().find(|c| c.0 == container_id) {
            c.3 = true;
        }
        Ok(())
    }

    async fn find_container(&self, sandbox_id: &str, name: &str) -> Result<Option<String>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .containers
            .iter()
            .find(|c| c.1 == sandbox_id && c.2 == name)
            .map(|c| c.0.clone()))
    }

    async fn container_state(&self, container_id: &str) -> Result<crate::ContainerState> {
        let s = self.state.lock().unwrap();
        Ok(match s.containers.iter().find(|c| c.0 == container_id) {
            Some(c) if c.3 => crate::ContainerState::Running,
            Some(_) => crate::ContainerState::Created,
            None => crate::ContainerState::Unknown,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_round_trip() {
        let f = FakeCriClient::new()
            .with_version("containerd", "2.0")
            .with_ready(true);
        assert_eq!(f.version().await.unwrap().runtime_name, "containerd");
        assert!(f.ready().await.unwrap());
        assert_eq!(f.calls(), 2); // both version() and ready() count

        let unreachable = FakeCriClient::new();
        assert!(unreachable.version().await.is_err());
        assert!(unreachable.ready().await.is_err());
    }

    #[tokio::test]
    async fn fake_image_presence_and_pull() {
        let f = FakeCriClient::new()
            .with_version("containerd", "2.0")
            .with_image("busybox:1.36");
        assert!(f.image_present("busybox:1.36").await.unwrap());
        assert!(!f.image_present("nope:1").await.unwrap());
        // pull makes a previously-absent image present.
        f.pull_image("pulled:1").await.unwrap();
        assert!(f.image_present("pulled:1").await.unwrap());
    }

    #[tokio::test]
    async fn fake_sandbox_create_is_idempotent_by_name() {
        let f = FakeCriClient::new().with_version("containerd", "2.0");
        let spec = crate::PodSpec {
            name: "hello".into(),
            uid: "u-hello".into(),
            host_network: true,
        };
        assert!(f.find_sandbox("hello").await.unwrap().is_none());
        let id = f.run_pod_sandbox(&spec).await.unwrap();
        assert_eq!(
            f.find_sandbox("hello").await.unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(f.sandbox_count(), 1);
    }

    #[tokio::test]
    async fn fake_container_lifecycle() {
        use crate::{ContainerSpec, ContainerState, PodSpec};
        let f = FakeCriClient::new()
            .with_version("containerd", "2.0")
            .with_image("busybox:1.36");
        let sb = f
            .run_pod_sandbox(&PodSpec {
                name: "hello".into(),
                uid: "u".into(),
                host_network: true,
            })
            .await
            .unwrap();
        let cspec = ContainerSpec {
            name: "hello".into(),
            image: "busybox:1.36".into(),
            command: vec!["/bin/sh".into(), "-c".into()],
            args: vec!["sleep 3600".into()],
        };
        assert!(f.find_container(&sb, "hello").await.unwrap().is_none());
        let id = f.create_container(&sb, &cspec).await.unwrap();
        assert_eq!(
            f.container_state(&id).await.unwrap(),
            ContainerState::Created
        );
        f.start_container(&id).await.unwrap();
        assert_eq!(
            f.container_state(&id).await.unwrap(),
            ContainerState::Running
        );
        assert_eq!(
            f.find_container(&sb, "hello").await.unwrap().as_deref(),
            Some(id.as_str())
        );
    }
}
