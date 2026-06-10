//! Controller abstraction and the [`Runtime`] that drives reconcile loops.

use std::time::Duration;

use async_trait::async_trait;
use machined_resources::ResourceType;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::error::Result;
use crate::state::State;
use crate::watch::Event;

/// Dependency strength of a controller input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// Depends-on: the controller is notified of teardown via finalizers.
    Strong,
    /// Watch-only: changes wake the controller but imply no ownership.
    Weak,
}

/// Ownership of a controller output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    /// Exactly one controller writes this type.
    Exclusive,
    /// Multiple controllers create objects of this type, each owning its own.
    Shared,
}

/// A declared input: a resource type (in a namespace) the controller watches.
#[derive(Clone, Debug)]
pub struct Input {
    pub namespace: String,
    pub typ: ResourceType,
    pub kind: InputKind,
}

/// A declared output: a resource type the controller writes.
#[derive(Clone, Debug)]
pub struct Output {
    pub typ: ResourceType,
    pub kind: OutputKind,
}

/// Context handed to a controller on each reconcile: the shared store.
pub struct ReconcileCtx {
    pub state: State,
}

/// A single-purpose reconciler. `reconcile` is called once at startup and
/// again whenever any declared input changes.
#[async_trait]
pub trait Controller: Send {
    fn name(&self) -> &str;
    fn inputs(&self) -> Vec<Input>;
    fn outputs(&self) -> Vec<Output>;
    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> Result<()>;
}

/// Registers controllers and drives their reconcile loops over a shared store.
pub struct Runtime {
    state: State,
    controllers: Vec<Box<dyn Controller>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            state: State::new(),
            controllers: Vec::new(),
        }
    }

    /// Build a runtime over an existing store (so callers can pre-seed it or
    /// share it with the API server).
    pub fn with_state(state: State) -> Self {
        Self {
            state,
            controllers: Vec::new(),
        }
    }

    pub fn state(&self) -> State {
        self.state.clone()
    }

    pub fn register(&mut self, controller: Box<dyn Controller>) {
        self.controllers.push(controller);
    }

    /// Spawn one reconcile loop per controller and run until `shutdown` fires.
    /// Returns when all loops have stopped.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let mut handles = Vec::new();
        for controller in self.controllers {
            let state = self.state.clone();
            let token = shutdown.clone();
            handles.push(tokio::spawn(controller_loop(controller, state, token)));
        }
        for h in handles {
            if let Err(e) = h.await {
                error!("controller task panicked: {e}");
            }
        }
        Ok(())
    }
}

/// Debounce window: after a wake, drain any immediately-pending events before
/// reconciling so a burst collapses into one reconcile pass.
const DEBOUNCE: Duration = Duration::from_millis(20);

async fn controller_loop(
    mut controller: Box<dyn Controller>,
    state: State,
    shutdown: CancellationToken,
) {
    let ctx = ReconcileCtx {
        state: state.clone(),
    };
    let inputs = controller.inputs();
    let mut rx = state.watch();

    // Initial reconcile.
    reconcile_once(&mut controller, &ctx).await;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                return;
            }
            recv = rx.recv() => {
                match recv {
                    Ok(event) => {
                        if !matches_inputs(&inputs, &event) {
                            continue;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(controller = controller.name(), skipped = n, "watch lagged; forcing reconcile");
                        // Fall through to reconcile — a full re-list is the cure.
                    }
                    Err(RecvError::Closed) => return,
                }
                // Debounce: collapse a burst into a single reconcile.
                tokio::time::sleep(DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
                reconcile_once(&mut controller, &ctx).await;
            }
        }
    }
}

fn matches_inputs(inputs: &[Input], event: &Event) -> bool {
    inputs.iter().any(|i| {
        i.typ == event.resource_type() && i.namespace == event.namespace()
    })
}

async fn reconcile_once(controller: &mut Box<dyn Controller>, ctx: &ReconcileCtx) {
    if let Err(e) = controller.reconcile(ctx).await {
        error!(controller = controller.name(), error = %e, "reconcile failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{
        Key, Resource, ResourceObject, ServiceState, ServiceStatusSpec,
    };

    /// A toy controller: for every ServiceStatus in `Failed` state, it records
    /// a finalizer-free marker by flipping `healthy` to false via update.
    struct HealthMarker;

    #[async_trait]
    impl Controller for HealthMarker {
        fn name(&self) -> &str {
            "health-marker"
        }
        fn inputs(&self) -> Vec<Input> {
            vec![Input {
                namespace: "runtime".into(),
                typ: ResourceType::ServiceStatus,
                kind: InputKind::Weak,
            }]
        }
        fn outputs(&self) -> Vec<Output> {
            vec![Output {
                typ: ResourceType::ServiceStatus,
                kind: OutputKind::Exclusive,
            }]
        }
        async fn reconcile(&mut self, ctx: &ReconcileCtx) -> Result<()> {
            for obj in ctx.state.list("runtime", ResourceType::ServiceStatus) {
                if let Resource::ServiceStatus(ref s) = obj.spec {
                    if s.state == ServiceState::Failed && s.healthy {
                        let mut new = s.clone();
                        new.healthy = false;
                        // Ignore conflicts; a later event re-reconciles.
                        let _ = ctx.state.update(
                            &obj.metadata.key(),
                            obj.metadata.version,
                            Resource::ServiceStatus(new),
                        );
                    }
                }
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn controller_reacts_to_input_change() {
        let mut rt = Runtime::new();
        let state = rt.state();
        rt.register(Box::new(HealthMarker));

        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let handle = tokio::spawn(async move { rt.run(token).await });

        // Seed a failed service after the runtime is up.
        state
            .create(ResourceObject::new(
                "runtime",
                "etcd",
                Resource::ServiceStatus(ServiceStatusSpec {
                    service_id: "etcd".into(),
                    state: ServiceState::Failed,
                    healthy: true,
                    last_message: "boom".into(),
                }),
            ))
            .unwrap();

        // Poll until the controller has flipped healthy=false.
        let key = Key::new("runtime", ResourceType::ServiceStatus, "etcd");
        let mut flipped = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Ok(obj) = state.get(&key) {
                if let Resource::ServiceStatus(s) = obj.spec {
                    if !s.healthy {
                        flipped = true;
                        break;
                    }
                }
            }
        }
        assert!(flipped, "controller never reconciled the failed service");

        shutdown.cancel();
        handle.await.unwrap().unwrap();
    }
}
