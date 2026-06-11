//! In-memory CRI client for tests.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{CriClient, CriError, Result, RuntimeVersion};

#[derive(Default)]
struct FakeState {
    version: Option<RuntimeVersion>,
    ready: bool,
    calls: usize,
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
        let s = self.state.lock().unwrap();
        if s.version.is_none() {
            return Err(CriError::Connect("unreachable".into()));
        }
        Ok(s.ready)
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
        assert_eq!(f.calls(), 1);

        let unreachable = FakeCriClient::new();
        assert!(unreachable.version().await.is_err());
        assert!(unreachable.ready().await.is_err());
    }
}
