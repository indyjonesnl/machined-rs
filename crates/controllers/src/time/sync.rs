//! Periodically syncs the clock via SNTP and publishes TimeStatus.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use machined_config::Provider;
use machined_resources::{Resource, ResourceObject, ResourceType, TimeStatus};
use machined_runtime_core::{reconcile_owned, Controller, Input, Output, OutputKind, ReconcileCtx};
use machined_time::TimeSync;
use tracing::warn;

use super::{ctl, NS};

const OWNER: &str = "time-sync";

/// Step the clock only when the offset exceeds this (128 ms, in ns).
const STEP_THRESHOLD_NS: i128 = 128_000_000;

/// Default NTP servers when config supplies none.
const DEFAULT_SERVERS: [&str; 2] = ["0.pool.ntp.org", "1.pool.ntp.org"];

pub struct TimeSyncController {
    sync: Arc<dyn TimeSync>,
    provider: Provider,
    sync_count: u64,
}

impl TimeSyncController {
    pub fn new(sync: Arc<dyn TimeSync>, provider: Provider) -> Self {
        Self {
            sync,
            provider,
            sync_count: 0,
        }
    }
}

fn status_obj(synced: bool, server: &str, offset_ns: i64, sync_count: u64) -> ResourceObject {
    ResourceObject::new(
        NS,
        "time",
        Resource::TimeStatus(TimeStatus {
            synced,
            server: server.to_string(),
            offset_ns,
            sync_count,
        }),
    )
}

#[async_trait]
impl Controller for TimeSyncController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        Vec::new()
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::TimeStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    fn resync_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(11 * 60))
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let time_cfg = self.provider.time();
        if time_cfg.disabled {
            reconcile_owned(
                &ctx.state,
                OWNER,
                NS,
                ResourceType::TimeStatus,
                vec![status_obj(false, "", 0, self.sync_count)],
            )?;
            return Ok(());
        }

        let servers: Vec<String> = if time_cfg.servers.is_empty() {
            DEFAULT_SERVERS.iter().map(|s| s.to_string()).collect()
        } else {
            time_cfg.servers.clone()
        };

        for server in &servers {
            let addr = format!("{server}:123");
            match self.sync.query_offset(&addr).await {
                Ok(offset) => {
                    if offset.abs() > STEP_THRESHOLD_NS {
                        self.sync.step_clock(offset).map_err(ctl)?;
                    }
                    self.sync_count += 1;
                    reconcile_owned(
                        &ctx.state,
                        OWNER,
                        NS,
                        ResourceType::TimeStatus,
                        vec![status_obj(true, server, offset as i64, self.sync_count)],
                    )?;
                    return Ok(());
                }
                Err(e) => warn!(server = %server, error = %e, "ntp query failed; trying next"),
            }
        }

        // No server answered — transient; the timer retries. Report not-synced.
        reconcile_owned(
            &ctx.state,
            OWNER,
            NS,
            ResourceType::TimeStatus,
            vec![status_obj(false, "", 0, self.sync_count)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::{MachineConfig, MachineSection, TimeSection};
    use machined_resources::Key;
    use machined_runtime_core::{ReconcileCtx, State};
    use machined_time::FakeTimeSync;

    fn provider(servers: Vec<&str>, disabled: bool) -> Provider {
        Provider::new(MachineConfig {
            machine: MachineSection {
                hostname: None,
                sysctls: vec![],
                services: vec![],
                network: Default::default(),
                install: None,
                time: TimeSection {
                    servers: servers.into_iter().map(|s| s.to_string()).collect(),
                    disabled,
                },
            },
        })
    }

    fn time_status(state: &State) -> TimeStatus {
        match state
            .get(&Key::new(NS, ResourceType::TimeStatus, "time"))
            .unwrap()
            .spec
        {
            Resource::TimeStatus(t) => t,
            _ => panic!("wrong type"),
        }
    }

    #[tokio::test]
    async fn syncs_and_steps_past_threshold() {
        let fake = Arc::new(FakeTimeSync::new().with_offset("a:123", 500_000_000)); // 500ms
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = TimeSyncController::new(fake.clone(), provider(vec!["a"], false));
        c.reconcile(&ctx).await.unwrap();

        assert_eq!(fake.steps(), vec![500_000_000]); // stepped
        let st = time_status(&state);
        assert!(st.synced);
        assert_eq!(st.server, "a");
        assert_eq!(st.sync_count, 1);
    }

    #[tokio::test]
    async fn small_offset_is_not_stepped() {
        let fake = Arc::new(FakeTimeSync::new().with_offset("a:123", 1_000_000)); // 1ms < 128ms
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = TimeSyncController::new(fake.clone(), provider(vec!["a"], false));
        c.reconcile(&ctx).await.unwrap();
        assert!(fake.steps().is_empty());
        assert!(time_status(&state).synced);
    }

    #[tokio::test]
    async fn falls_over_to_second_server() {
        // First server unreachable (no offset), second answers → synced on "b".
        let fake = Arc::new(FakeTimeSync::new().with_offset("b:123", 500_000_000));
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = TimeSyncController::new(fake.clone(), provider(vec!["a", "b"], false));
        c.reconcile(&ctx).await.unwrap();
        let st = time_status(&state);
        assert!(st.synced);
        assert_eq!(st.server, "b");
        assert_eq!(fake.steps(), vec![500_000_000]);
    }

    #[tokio::test]
    async fn disabled_does_not_query() {
        let fake = Arc::new(FakeTimeSync::new().with_offset("a:123", 500_000_000));
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = TimeSyncController::new(fake.clone(), provider(vec!["a"], true));
        c.reconcile(&ctx).await.unwrap();
        assert!(fake.steps().is_empty());
        assert!(!time_status(&state).synced);
    }

    #[tokio::test]
    async fn all_unreachable_reports_unsynced() {
        let fake = Arc::new(FakeTimeSync::new()); // no offsets → all error
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = TimeSyncController::new(fake.clone(), provider(vec!["a", "b"], false));
        c.reconcile(&ctx).await.unwrap(); // Ok, not Err
        assert!(!time_status(&state).synced);
    }
}
