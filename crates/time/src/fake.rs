//! In-memory `TimeSync` for root-free tests.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{Result, TimeError, TimeOffset, TimeSync};

#[derive(Default)]
struct FakeState {
    /// Per-addr canned offset; absent → query errors (unreachable).
    offsets: HashMap<String, TimeOffset>,
    /// Recorded step_clock calls.
    steps: Vec<TimeOffset>,
}

#[derive(Default)]
pub struct FakeTimeSync {
    state: Mutex<FakeState>,
}

impl FakeTimeSync {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make `addr` answer with `offset`.
    pub fn with_offset(self, addr: &str, offset: TimeOffset) -> Self {
        self.state
            .lock()
            .unwrap()
            .offsets
            .insert(addr.to_string(), offset);
        self
    }

    /// Recorded step_clock offsets.
    pub fn steps(&self) -> Vec<TimeOffset> {
        self.state.lock().unwrap().steps.clone()
    }
}

#[async_trait]
impl TimeSync for FakeTimeSync {
    async fn query_offset(&self, addr: &str) -> Result<TimeOffset> {
        self.state
            .lock()
            .unwrap()
            .offsets
            .get(addr)
            .copied()
            .ok_or_else(|| TimeError::Io(format!("unreachable: {addr}")))
    }

    fn step_clock(&self, offset: TimeOffset) -> Result<()> {
        self.state.lock().unwrap().steps.push(offset);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_configured_addr_and_records_steps() {
        let f = FakeTimeSync::new().with_offset("a:123", 5_000);
        assert_eq!(f.query_offset("a:123").await.unwrap(), 5_000);
        assert!(f.query_offset("b:123").await.is_err());
        f.step_clock(5_000).unwrap();
        assert_eq!(f.steps(), vec![5_000]);
    }
}
