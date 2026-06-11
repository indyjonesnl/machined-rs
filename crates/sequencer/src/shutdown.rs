//! The Shutdown sequence: stop services in reverse order, then unmount/halt.
//! (Unmount + halt are no-ops on the fake platform and full ops under Linux;
//! M5 fleshes out disk teardown and final reboot/poweroff.)

use async_trait::async_trait;

use crate::task::{PhaseList, SequencerCtx, Task};

struct StopServices;

#[async_trait]
impl Task for StopServices {
    fn name(&self) -> &str {
        "stop-services"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        ctx.services.lock().await.stop_all().await;
        Ok(())
    }
}

/// Build the Shutdown phase list.
pub fn shutdown_sequence() -> PhaseList {
    PhaseList::new().phase("stop", vec![Box::new(StopServices)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::{MachineConfig, Provider};
    use machined_platform::FakePlatform;
    use machined_runtime_core::State;
    use machined_supervisor::{DefaultReadiness, ServiceManager};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn shutdown_runs_clean() {
        let state = State::new();
        let ctx = SequencerCtx {
            state: state.clone(),
            platform: Arc::new(FakePlatform::new()),
            provider: Provider::new(MachineConfig::default()),
            services: Arc::new(Mutex::new(ServiceManager::new(state))),
            readiness: Arc::new(DefaultReadiness),
        };
        shutdown_sequence().run(&ctx).await.unwrap();
    }
}
