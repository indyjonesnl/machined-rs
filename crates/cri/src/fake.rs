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
}
