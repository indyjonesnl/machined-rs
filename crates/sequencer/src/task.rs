//! The phase/task model: ordered, idempotent steps run during a lifecycle
//! sequence (boot, shutdown, ...).

use std::sync::Arc;

use async_trait::async_trait;
use machined_config::Provider;
use machined_platform::Platform;
use machined_runtime_core::State;
use machined_supervisor::ServiceManager;
use tokio::sync::Mutex;
use tracing::info;

#[derive(thiserror::Error, Debug)]
#[error("task {task} failed: {message}")]
pub struct TaskError {
    pub task: String,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, TaskError>;

/// Shared context handed to every task.
#[derive(Clone)]
pub struct SequencerCtx {
    pub state: State,
    pub platform: Arc<dyn Platform>,
    pub provider: Provider,
    pub services: Arc<Mutex<ServiceManager>>,
}

/// A single idempotent step in a sequence.
#[async_trait]
pub trait Task: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, ctx: &SequencerCtx) -> Result<()>;
}

/// A named, ordered group of tasks.
pub struct Phase {
    pub name: String,
    pub tasks: Vec<Box<dyn Task>>,
}

/// An ordered list of phases.
pub struct PhaseList {
    pub phases: Vec<Phase>,
}

impl PhaseList {
    pub fn new() -> Self {
        Self { phases: Vec::new() }
    }

    pub fn phase(mut self, name: &str, tasks: Vec<Box<dyn Task>>) -> Self {
        self.phases.push(Phase {
            name: name.to_string(),
            tasks,
        });
        self
    }

    /// Run every phase's tasks in order. Stops at the first task error.
    pub async fn run(&self, ctx: &SequencerCtx) -> Result<()> {
        for phase in &self.phases {
            info!(phase = %phase.name, "entering phase");
            for task in &phase.tasks {
                info!(phase = %phase.name, task = task.name(), "running task");
                task.run(ctx).await?;
            }
        }
        Ok(())
    }
}

impl Default for PhaseList {
    fn default() -> Self {
        Self::new()
    }
}
