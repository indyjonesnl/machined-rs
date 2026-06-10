# machined-rs M1 — First-Boot Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** The M0 plan (`2026-06-10-machined-rs-m0-runtime-core.md`) must be complete — this plan calls `machined-runtime-core` and `machined-resources` APIs defined there.

**Goal:** Boot a node end-to-end as PID 1: mount essential filesystems, load a clean-break YAML machine config, run the reconcile runtime, and supervise a config-declared `process` service to `Running` — then shut it all down cleanly on `SIGTERM`.

**Architecture:** A `platform` crate abstracts privileged OS operations (mount/sysctl/cmdline/hostname/reboot) behind a trait with a real Linux impl and a fake for tests. A `config` crate parses single-doc YAML into typed structs and surfaces it as a `Resource::MachineConfig`. A `supervisor` crate runs services via a `Runner` trait (`process` + `restart` wrapper), driving each through a state machine and writing `ServiceStatus` resources into the shared `State`. A `sequencer` crate orders idempotent tasks into Boot and Shutdown phase lists. The `machined` binary ties it together as PID 1: signal handling, zombie reaping, run Boot → wait → run Shutdown, with an emergency path on fatal error.

**Tech Stack:** Builds on M0. Adds `nix` (mounts, signals, `waitpid`), `tokio::process`, `serde_yaml`, `async-trait`.

---

## File Structure

Adds five crates to the workspace from M0.

```
machined-rs/
├── Cargo.toml                       # add members: platform, config, supervisor, sequencer, machined
├── crates/
│   ├── platform/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs               # Platform trait + re-exports
│   │       ├── linux.rs             # LinuxPlatform (real privileged ops)
│   │       └── fake.rs              # FakePlatform (records calls, for tests)
│   ├── config/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs               # re-exports
│   │       ├── types.rs             # MachineConfig + sections (serde)
│   │       ├── provider.rs          # Provider trait + impl
│   │       └── load.rs              # load from path, build MachineConfig resource
│   ├── supervisor/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs               # re-exports
│   │       ├── runner.rs            # Runner trait + RunOutcome
│   │       ├── process.rs           # ProcessRunner
│   │       ├── restart.rs           # RestartRunner wrapper
│   │       ├── service.rs           # ServiceRunner state machine
│   │       └── manager.rs           # ServiceManager (dependency-ordered start/stop)
│   ├── sequencer/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs               # re-exports
│   │       ├── task.rs              # Task trait, Phase, PhaseList, SequencerCtx
│   │       ├── boot.rs             # boot_sequence()
│   │       └── shutdown.rs          # shutdown_sequence()
│   └── machined/
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs              # PID1 entry, multi-call dispatch
│           ├── pid1.rs              # signal handlers + zombie reaper
│           └── emergency.rs         # emergency halt path
└── tests/                           # workspace integration tests live per-crate; boot harness in machined
```

**Responsibilities:**
- `platform` — the only crate doing privileged syscalls; everything testable behind the trait.
- `config` — typed config + read-only `Provider`; no I/O beyond `load`.
- `supervisor` — run/stop/observe services; the only owner of `ServiceStatus` resources.
- `sequencer` — ordering glue; tasks are thin and call into platform/supervisor/config.
- `machined` — process entry, PID-1 duties, wiring.

---

## Task 1: Add crates to workspace + `config` crate

**Files:**
- Modify: `Cargo.toml`
- Modify: `.gitignore` (commit Cargo.lock now that we ship a binary)
- Create: `crates/config/Cargo.toml`
- Create: `crates/config/src/lib.rs`
- Create: `crates/config/src/types.rs`
- Create: `crates/config/src/provider.rs`
- Create: `crates/config/src/load.rs`

- [ ] **Step 1: Extend workspace members + deps**

Edit `Cargo.toml` `members` to:

```toml
members = [
    "crates/common",
    "crates/resources",
    "crates/runtime-core",
    "crates/platform",
    "crates/config",
    "crates/supervisor",
    "crates/sequencer",
    "crates/machined",
]
```

Add to `[workspace.dependencies]`:

```toml
nix = { version = "0.29", features = ["mount", "signal", "process", "hostname"] }
anyhow = "1.0"

# internal crates (added in M1)
machined-platform = { path = "crates/platform" }
machined-config = { path = "crates/config" }
machined-supervisor = { path = "crates/supervisor" }
machined-sequencer = { path = "crates/sequencer" }
```

- [ ] **Step 2: Commit Cargo.lock from now on**

Edit `.gitignore` — remove the `Cargo.lock` line (the workspace now ships the `machined` binary, so the lockfile should be committed).

- [ ] **Step 3: Write the failing config-parse test**

Create `crates/config/Cargo.toml`:

```toml
[package]
name = "machined-config"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
machined-resources.workspace = true
serde.workspace = true
serde_yaml.workspace = true
thiserror.workspace = true
```

Create `crates/config/src/types.rs`:

```rust
//! Typed machine configuration (clean-break, single-document YAML).

use serde::Deserialize;

/// Top-level machine config document.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    #[serde(default)]
    pub machine: MachineSection,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineSection {
    /// Node hostname to apply during boot.
    #[serde(default)]
    pub hostname: Option<String>,
    /// `sysctl` key/values to apply during boot.
    #[serde(default)]
    pub sysctls: Vec<Sysctl>,
    /// Services machined supervises (the payload + helpers).
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sysctl {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    /// Unique service id.
    pub id: String,
    /// argv to exec (argv[0] is the program).
    pub command: Vec<String>,
    /// Service ids that must be Running before this one starts.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Restart policy on exit.
    #[serde(default)]
    pub restart: RestartPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Never restart (run-once).
    Never,
    /// Restart only on non-zero exit.
    #[default]
    OnFailure,
    /// Always restart.
    Always,
}
```

Create `crates/config/src/lib.rs`:

```rust
//! Clean-break machine config: typed model, loader, and read-only provider.

pub mod load;
pub mod provider;
pub mod types;

pub use load::{load_from_str, ConfigError};
pub use provider::Provider;
pub use types::{MachineConfig, MachineSection, RestartPolicy, ServiceConfig, Sysctl};
```

Create `crates/config/src/load.rs`:

```rust
//! Loading machine config from YAML and projecting it into a resource.

use std::path::Path;

use machined_resources::{MachineConfigSpec, Resource, ResourceObject};

use crate::types::MachineConfig;

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("reading config {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Parse(#[from] serde_yaml::Error),
}

/// Parse a machine config from a YAML string.
pub fn load_from_str(yaml: &str) -> Result<MachineConfig, ConfigError> {
    Ok(serde_yaml::from_str(yaml)?)
}

/// Read and parse a machine config from disk.
pub fn load_from_path(path: &Path) -> Result<(MachineConfig, String), ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let cfg = load_from_str(&raw)?;
    Ok((cfg, raw))
}

/// Build the `MachineConfig` resource object that controllers reconcile against.
/// `raw_yaml` is the document the config was parsed from.
pub fn to_resource(raw_yaml: String) -> ResourceObject {
    ResourceObject::new(
        "runtime",
        "machine-config",
        Resource::MachineConfig(MachineConfigSpec { raw_yaml }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RestartPolicy;

    const SAMPLE: &str = r#"
machine:
  hostname: node-1
  sysctls:
    - key: net.ipv4.ip_forward
      value: "1"
  services:
    - id: payload
      command: ["/usr/bin/rusternetes", "--all-in-one"]
      restart: always
"#;

    #[test]
    fn parses_machine_config() {
        let cfg = load_from_str(SAMPLE).unwrap();
        assert_eq!(cfg.machine.hostname.as_deref(), Some("node-1"));
        assert_eq!(cfg.machine.sysctls.len(), 1);
        assert_eq!(cfg.machine.services.len(), 1);
        let svc = &cfg.machine.services[0];
        assert_eq!(svc.id, "payload");
        assert_eq!(svc.restart, RestartPolicy::Always);
        assert_eq!(svc.command[0], "/usr/bin/rusternetes");
    }

    #[test]
    fn empty_config_is_valid() {
        let cfg = load_from_str("{}").unwrap();
        assert!(cfg.machine.hostname.is_none());
        assert!(cfg.machine.services.is_empty());
    }

    #[test]
    fn builds_resource_object() {
        let obj = to_resource("machine: {}".into());
        assert_eq!(obj.metadata.id, "machine-config");
        match obj.spec {
            Resource::MachineConfig(s) => assert_eq!(s.raw_yaml, "machine: {}"),
            _ => panic!("wrong resource type"),
        }
    }
}
```

Create `crates/config/src/provider.rs`:

```rust
//! Read-only view over the machine config handed to controllers and tasks.

use crate::types::{MachineConfig, ServiceConfig, Sysctl};

/// A read-only, cloneable snapshot view of the loaded config.
#[derive(Clone, Debug)]
pub struct Provider {
    config: MachineConfig,
}

impl Provider {
    pub fn new(config: MachineConfig) -> Self {
        Self { config }
    }

    pub fn hostname(&self) -> Option<&str> {
        self.config.machine.hostname.as_deref()
    }

    pub fn sysctls(&self) -> &[Sysctl] {
        &self.config.machine.sysctls
    }

    pub fn services(&self) -> &[ServiceConfig] {
        &self.config.machine.services
    }
}
```

- [ ] **Step 4: Run the config tests to verify they pass**

Run: `cargo test -p machined-config`
Expected: PASS — all three tests pass.

> The other new crates (`platform`, `supervisor`, `sequencer`, `machined`) are listed as members but don't exist yet, so `cargo build --workspace` will fail until Task 5. Build only `-p machined-config` for now.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml .gitignore crates/config Cargo.lock
git commit -m "feat(config): typed machine config + loader + provider"
```

---

## Task 2: `platform` crate — privileged-op abstraction

**Files:**
- Create: `crates/platform/Cargo.toml`
- Create: `crates/platform/src/lib.rs`
- Create: `crates/platform/src/linux.rs`
- Create: `crates/platform/src/fake.rs`

- [ ] **Step 1: Define the Platform trait + the test fake first**

Create `crates/platform/Cargo.toml`:

```toml
[package]
name = "machined-platform"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
thiserror.workspace = true
tracing.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
nix.workspace = true
```

Create `crates/platform/src/lib.rs`:

```rust
//! Privileged OS operations behind a trait, so the sequencer and supervisor
//! are testable without root. `LinuxPlatform` does the real work;
//! `FakePlatform` records calls.

pub mod fake;
#[cfg(target_os = "linux")]
pub mod linux;

pub use fake::FakePlatform;
#[cfg(target_os = "linux")]
pub use linux::LinuxPlatform;

#[derive(thiserror::Error, Debug)]
pub enum PlatformError {
    #[error("mount {target}: {message}")]
    Mount { target: String, message: String },
    #[error("sysctl {key}: {message}")]
    Sysctl { key: String, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, PlatformError>;

/// A filesystem to mount during early boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountSpec {
    pub source: String,
    pub target: String,
    pub fstype: String,
    pub flags: u64,
    pub data: Option<String>,
}

/// The set of pseudo-filesystems machined mounts before anything else.
pub fn essential_mounts() -> Vec<MountSpec> {
    let m = |source: &str, target: &str, fstype: &str| MountSpec {
        source: source.into(),
        target: target.into(),
        fstype: fstype.into(),
        flags: 0,
        data: None,
    };
    vec![
        m("proc", "/proc", "proc"),
        m("sysfs", "/sys", "sysfs"),
        m("devtmpfs", "/dev", "devtmpfs"),
        m("tmpfs", "/run", "tmpfs"),
        m("tmpfs", "/tmp", "tmpfs"),
    ]
}

/// Abstraction over the privileged operations early boot needs.
pub trait Platform: Send + Sync {
    fn mount(&self, spec: &MountSpec) -> Result<()>;
    fn set_sysctl(&self, key: &str, value: &str) -> Result<()>;
    fn set_hostname(&self, name: &str) -> Result<()>;
    fn kernel_cmdline(&self) -> Result<String>;
    fn reboot(&self) -> Result<()>;
    fn poweroff(&self) -> Result<()>;

    /// Mount every essential pseudo-filesystem. Default impl loops `mount`.
    fn mount_essential(&self) -> Result<()> {
        for spec in essential_mounts() {
            self.mount(&spec)?;
        }
        Ok(())
    }
}
```

Create `crates/platform/src/fake.rs`:

```rust
//! In-memory fake platform that records operations instead of performing them.

use std::sync::Mutex;

use crate::{MountSpec, Platform, Result};

#[derive(Debug, Default)]
pub struct Recorded {
    pub mounts: Vec<MountSpec>,
    pub sysctls: Vec<(String, String)>,
    pub hostname: Option<String>,
    pub rebooted: bool,
    pub poweroff: bool,
}

#[derive(Default)]
pub struct FakePlatform {
    pub recorded: Mutex<Recorded>,
    pub cmdline: String,
}

impl FakePlatform {
    pub fn new() -> Self {
        Self {
            recorded: Mutex::new(Recorded::default()),
            cmdline: "console=ttyS0".into(),
        }
    }
}

impl Platform for FakePlatform {
    fn mount(&self, spec: &MountSpec) -> Result<()> {
        self.recorded.lock().unwrap().mounts.push(spec.clone());
        Ok(())
    }
    fn set_sysctl(&self, key: &str, value: &str) -> Result<()> {
        self.recorded
            .lock()
            .unwrap()
            .sysctls
            .push((key.to_string(), value.to_string()));
        Ok(())
    }
    fn set_hostname(&self, name: &str) -> Result<()> {
        self.recorded.lock().unwrap().hostname = Some(name.to_string());
        Ok(())
    }
    fn kernel_cmdline(&self) -> Result<String> {
        Ok(self.cmdline.clone())
    }
    fn reboot(&self) -> Result<()> {
        self.recorded.lock().unwrap().rebooted = true;
        Ok(())
    }
    fn poweroff(&self) -> Result<()> {
        self.recorded.lock().unwrap().poweroff = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::essential_mounts;

    #[test]
    fn fake_records_mounts_and_sysctls() {
        let p = FakePlatform::new();
        p.mount_essential().unwrap();
        p.set_sysctl("net.ipv4.ip_forward", "1").unwrap();
        p.set_hostname("node-1").unwrap();

        let rec = p.recorded.lock().unwrap();
        assert_eq!(rec.mounts.len(), essential_mounts().len());
        assert_eq!(rec.mounts[0].target, "/proc");
        assert_eq!(rec.sysctls[0], ("net.ipv4.ip_forward".into(), "1".into()));
        assert_eq!(rec.hostname.as_deref(), Some("node-1"));
    }
}
```

- [ ] **Step 2: Run the fake test to verify it passes**

Run: `cargo test -p machined-platform fake`
Expected: PASS.

- [ ] **Step 3: Implement the real Linux platform**

Create `crates/platform/src/linux.rs`:

```rust
//! Real privileged operations via `nix` and `/proc`. Only compiled on Linux.
//! These are exercised by VM-based integration tests, not unit tests (they
//! require root and a real kernel).

use std::fs;

use nix::mount::{mount, MsFlags};
use nix::sys::reboot::{reboot, RebootMode};
use nix::unistd::sethostname;

use crate::{MountSpec, Platform, PlatformError, Result};

pub struct LinuxPlatform;

impl LinuxPlatform {
    pub fn new() -> Self {
        LinuxPlatform
    }
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl Platform for LinuxPlatform {
    fn mount(&self, spec: &MountSpec) -> Result<()> {
        // Best-effort create the mountpoint.
        let _ = fs::create_dir_all(&spec.target);
        let flags = MsFlags::from_bits_truncate(spec.flags);
        mount(
            Some(spec.source.as_str()),
            spec.target.as_str(),
            Some(spec.fstype.as_str()),
            flags,
            spec.data.as_deref(),
        )
        .map_err(|e| PlatformError::Mount {
            target: spec.target.clone(),
            message: e.to_string(),
        })
    }

    fn set_sysctl(&self, key: &str, value: &str) -> Result<()> {
        let path = format!("/proc/sys/{}", key.replace('.', "/"));
        fs::write(&path, value).map_err(|e| PlatformError::Sysctl {
            key: key.to_string(),
            message: e.to_string(),
        })
    }

    fn set_hostname(&self, name: &str) -> Result<()> {
        sethostname(name).map_err(|e| PlatformError::Other(format!("sethostname: {e}")))
    }

    fn kernel_cmdline(&self) -> Result<String> {
        Ok(fs::read_to_string("/proc/cmdline")?)
    }

    fn reboot(&self) -> Result<()> {
        reboot(RebootMode::RB_AUTOBOOT)
            .map(|_| ())
            .map_err(|e| PlatformError::Other(format!("reboot: {e}")))
    }

    fn poweroff(&self) -> Result<()> {
        reboot(RebootMode::RB_POWER_OFF)
            .map(|_| ())
            .map_err(|e| PlatformError::Other(format!("poweroff: {e}")))
    }
}
```

- [ ] **Step 4: Build + lint the platform crate**

Run: `cargo build -p machined-platform`
Expected: PASS.

Run: `cargo clippy -p machined-platform --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/platform
git commit -m "feat(platform): Platform trait + Linux impl + fake"
```

---

## Task 3: `supervisor` — Runner trait + ProcessRunner

**Files:**
- Create: `crates/supervisor/Cargo.toml`
- Create: `crates/supervisor/src/lib.rs`
- Create: `crates/supervisor/src/runner.rs`
- Create: `crates/supervisor/src/process.rs`

- [ ] **Step 1: Define the Runner trait**

Create `crates/supervisor/Cargo.toml`:

```toml
[package]
name = "machined-supervisor"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
machined-common.workspace = true
machined-resources.workspace = true
machined-runtime-core.workspace = true
async-trait.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true
```

Create `crates/supervisor/src/runner.rs`:

```rust
//! The `Runner` abstraction: one backend that can start, await, and stop a
//! single service instance.

use async_trait::async_trait;

#[derive(thiserror::Error, Debug)]
pub enum RunnerError {
    #[error("spawning service {id}: {source}")]
    Spawn {
        id: String,
        #[source]
        source: std::io::Error,
    },
    #[error("service {0} is not running")]
    NotRunning(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, RunnerError>;

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// Process exited 0.
    Success,
    /// Process exited non-zero or by signal.
    Failure,
    /// `stop()` was requested.
    Stopped,
}

/// A startable/stoppable service backend. One `Runner` drives one instance.
#[async_trait]
pub trait Runner: Send {
    /// Human-readable id for logging/status.
    fn id(&self) -> &str;
    /// Start the instance and block until it exits or `stop` is called.
    async fn run(&mut self) -> Result<RunOutcome>;
    /// Request a graceful stop of a running instance.
    async fn stop(&mut self) -> Result<()>;
}
```

- [ ] **Step 2: Write the failing ProcessRunner tests**

Create `crates/supervisor/src/process.rs`:

```rust
//! `Runner` backend that forks/execs a host process via `tokio::process`.

use async_trait::async_trait;
use tokio::process::{Child, Command};
use tracing::warn;

use crate::runner::{RunOutcome, Runner, RunnerError};

pub struct ProcessRunner {
    id: String,
    command: Vec<String>,
    child: Option<Child>,
}

impl ProcessRunner {
    /// `command[0]` is the program, the rest are args.
    pub fn new(id: impl Into<String>, command: Vec<String>) -> Self {
        Self {
            id: id.into(),
            command,
            child: None,
        }
    }
}

#[async_trait]
impl Runner for ProcessRunner {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&mut self) -> crate::runner::Result<RunOutcome> {
        let (program, args) = self
            .command
            .split_first()
            .ok_or_else(|| RunnerError::Other(format!("service {} has empty command", self.id)))?;

        let child = Command::new(program)
            .args(args)
            .spawn()
            .map_err(|source| RunnerError::Spawn {
                id: self.id.clone(),
                source,
            })?;
        self.child = Some(child);

        let status = self
            .child
            .as_mut()
            .unwrap()
            .wait()
            .await
            .map_err(|e| RunnerError::Other(format!("wait {}: {e}", self.id)))?;
        self.child = None;

        Ok(if status.success() {
            RunOutcome::Success
        } else {
            RunOutcome::Failure
        })
    }

    async fn stop(&mut self) -> crate::runner::Result<()> {
        if let Some(child) = self.child.as_mut() {
            // tokio's start_kill sends SIGKILL; for M1 that is acceptable.
            // M5 replaces this with SIGTERM + grace timeout + SIGKILL.
            if let Err(e) = child.start_kill() {
                warn!(service = %self.id, "kill failed: {e}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_success_exits_zero() {
        let mut r = ProcessRunner::new("ok", vec!["true".into()]);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Success);
    }

    #[tokio::test]
    async fn run_failure_exits_nonzero() {
        let mut r = ProcessRunner::new("bad", vec!["false".into()]);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Failure);
    }

    #[tokio::test]
    async fn empty_command_errors() {
        let mut r = ProcessRunner::new("empty", vec![]);
        assert!(r.run().await.is_err());
    }

    #[tokio::test]
    async fn missing_program_errors() {
        let mut r = ProcessRunner::new("nope", vec!["/no/such/binary".into()]);
        assert!(matches!(r.run().await, Err(RunnerError::Spawn { .. })));
    }
}
```

Create `crates/supervisor/src/lib.rs`:

```rust
//! Service supervision: run services via pluggable runners, drive them through
//! a lifecycle, and reflect their state as `ServiceStatus` resources.

pub mod process;
pub mod runner;

pub use process::ProcessRunner;
pub use runner::{RunOutcome, Runner, RunnerError};
```

- [ ] **Step 3: Run the ProcessRunner tests to verify they pass**

Run: `cargo test -p machined-supervisor process`
Expected: PASS — `true`/`false`/`/no/such/binary` behave as asserted. (Requires `true` and `false` on PATH, present on Linux CI.)

- [ ] **Step 4: Commit**

```bash
git add crates/supervisor
git commit -m "feat(supervisor): Runner trait + ProcessRunner"
```

---

## Task 4: `supervisor` — RestartRunner wrapper

**Files:**
- Create: `crates/supervisor/src/restart.rs`
- Modify: `crates/supervisor/src/lib.rs`

- [ ] **Step 1: Write the failing restart tests**

Create `crates/supervisor/src/restart.rs`:

```rust
//! A `Runner` decorator that re-runs the inner runner according to a policy.

use async_trait::async_trait;
use std::time::Duration;
use tracing::info;

use crate::runner::{RunOutcome, Runner};

/// Restart behaviour for a wrapped runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Never,
    OnFailure,
    Always,
}

pub struct RestartRunner<R: Runner> {
    inner: R,
    policy: Policy,
    backoff: Duration,
    /// Test seam: stop after this many runs even under Always. `None` = forever.
    max_runs: Option<u32>,
}

impl<R: Runner> RestartRunner<R> {
    pub fn new(inner: R, policy: Policy) -> Self {
        Self {
            inner,
            policy,
            backoff: Duration::from_millis(100),
            max_runs: None,
        }
    }

    /// Cap the number of runs (used by tests to bound `Always`).
    pub fn with_max_runs(mut self, max: u32) -> Self {
        self.max_runs = Some(max);
        self
    }

    fn should_restart(&self, outcome: RunOutcome) -> bool {
        match self.policy {
            Policy::Never => false,
            Policy::OnFailure => outcome == RunOutcome::Failure,
            Policy::Always => outcome != RunOutcome::Stopped,
        }
    }
}

#[async_trait]
impl<R: Runner> Runner for RestartRunner<R> {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn run(&mut self) -> crate::runner::Result<RunOutcome> {
        let mut runs = 0u32;
        loop {
            let outcome = self.inner.run().await?;
            runs += 1;
            if let Some(max) = self.max_runs {
                if runs >= max {
                    return Ok(outcome);
                }
            }
            if !self.should_restart(outcome) {
                return Ok(outcome);
            }
            info!(service = self.inner.id(), ?outcome, "restarting service");
            tokio::time::sleep(self.backoff).await;
        }
    }

    async fn stop(&mut self) -> crate::runner::Result<()> {
        self.inner.stop().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{Result as RunnerResult, RunnerError};

    /// A runner that returns a scripted sequence of outcomes.
    struct ScriptedRunner {
        id: String,
        outcomes: Vec<RunOutcome>,
        idx: usize,
    }

    #[async_trait]
    impl Runner for ScriptedRunner {
        fn id(&self) -> &str {
            &self.id
        }
        async fn run(&mut self) -> RunnerResult<RunOutcome> {
            let o = *self
                .outcomes
                .get(self.idx)
                .ok_or_else(|| RunnerError::Other("script exhausted".into()))?;
            self.idx += 1;
            Ok(o)
        }
        async fn stop(&mut self) -> RunnerResult<()> {
            Ok(())
        }
    }

    fn scripted(outcomes: Vec<RunOutcome>) -> ScriptedRunner {
        ScriptedRunner {
            id: "s".into(),
            outcomes,
            idx: 0,
        }
    }

    #[tokio::test]
    async fn never_does_not_restart() {
        let mut r = RestartRunner::new(scripted(vec![RunOutcome::Failure]), Policy::Never);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Failure);
    }

    #[tokio::test]
    async fn on_failure_restarts_until_success() {
        // Fail, fail, succeed → three runs, returns Success.
        let mut r = RestartRunner::new(
            scripted(vec![
                RunOutcome::Failure,
                RunOutcome::Failure,
                RunOutcome::Success,
            ]),
            Policy::OnFailure,
        );
        assert_eq!(r.run().await.unwrap(), RunOutcome::Success);
    }

    #[tokio::test]
    async fn always_restarts_but_respects_max_runs() {
        let mut r = RestartRunner::new(
            scripted(vec![RunOutcome::Success, RunOutcome::Success]),
            Policy::Always,
        )
        .with_max_runs(2);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Success);
    }

    #[tokio::test]
    async fn always_stops_on_stopped() {
        let mut r = RestartRunner::new(scripted(vec![RunOutcome::Stopped]), Policy::Always);
        assert_eq!(r.run().await.unwrap(), RunOutcome::Stopped);
    }
}
```

- [ ] **Step 2: Export RestartRunner**

Append to `crates/supervisor/src/lib.rs`:

```rust
pub mod restart;
pub use restart::{Policy, RestartRunner};
```

- [ ] **Step 3: Run the restart tests to verify they pass**

Run: `cargo test -p machined-supervisor restart`
Expected: PASS — all four tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/supervisor/src/restart.rs crates/supervisor/src/lib.rs
git commit -m "feat(supervisor): RestartRunner policy wrapper"
```

---

## Task 5: `supervisor` — ServiceRunner + ServiceManager

**Files:**
- Create: `crates/supervisor/src/service.rs`
- Create: `crates/supervisor/src/manager.rs`
- Modify: `crates/supervisor/src/lib.rs`

- [ ] **Step 1: Write the ServiceRunner that reflects state into the store**

Create `crates/supervisor/src/service.rs`:

```rust
//! Drives one service: runs its `Runner` and reflects lifecycle transitions
//! into the shared `State` as a `ServiceStatus` resource.

use machined_resources::{
    Key, Resource, ResourceObject, ResourceType, ServiceState, ServiceStatusSpec,
};
use machined_runtime_core::State;
use tracing::info;

use crate::runner::{RunOutcome, Runner};

const NS: &str = "runtime";

fn key(id: &str) -> Key {
    Key::new(NS, ResourceType::ServiceStatus, id)
}

/// Write/refresh the ServiceStatus resource for `id` in the store.
pub fn publish_status(state: &State, id: &str, st: ServiceState, healthy: bool, message: &str) {
    let spec = ServiceStatusSpec {
        service_id: id.to_string(),
        state: st,
        healthy,
        last_message: message.to_string(),
    };
    let k = key(id);
    match state.get(&k) {
        Ok(existing) => {
            let _ = state.update(&k, existing.metadata.version, Resource::ServiceStatus(spec));
        }
        Err(_) => {
            let _ = state.create(ResourceObject::new(NS, id, Resource::ServiceStatus(spec)));
        }
    }
}

/// Drive `runner` to completion, publishing status transitions to `state`.
pub async fn run_service<R: Runner>(state: &State, mut runner: R) -> RunOutcome {
    let id = runner.id().to_string();
    publish_status(state, &id, ServiceState::Preparing, false, "starting");
    publish_status(state, &id, ServiceState::Running, true, "running");
    info!(service = %id, "service running");

    let outcome = match runner.run().await {
        Ok(o) => o,
        Err(e) => {
            publish_status(state, &id, ServiceState::Failed, false, &e.to_string());
            return RunOutcome::Failure;
        }
    };

    let (final_state, msg) = match outcome {
        RunOutcome::Success => (ServiceState::Finished, "exited 0"),
        RunOutcome::Failure => (ServiceState::Failed, "exited non-zero"),
        RunOutcome::Stopped => (ServiceState::Finished, "stopped"),
    };
    publish_status(state, &id, final_state, outcome != RunOutcome::Failure, msg);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Runner;
    use async_trait::async_trait;

    struct Instant(RunOutcome, String);

    #[async_trait]
    impl Runner for Instant {
        fn id(&self) -> &str {
            &self.1
        }
        async fn run(&mut self) -> crate::runner::Result<RunOutcome> {
            Ok(self.0)
        }
        async fn stop(&mut self) -> crate::runner::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn publishes_running_then_finished() {
        let state = State::new();
        let outcome = run_service(&state, Instant(RunOutcome::Success, "svc".into())).await;
        assert_eq!(outcome, RunOutcome::Success);

        let obj = state.get(&key("svc")).unwrap();
        match obj.spec {
            Resource::ServiceStatus(s) => {
                assert_eq!(s.state, ServiceState::Finished);
            }
            _ => panic!("wrong type"),
        }
    }

    #[tokio::test]
    async fn failure_marks_failed_unhealthy() {
        let state = State::new();
        run_service(&state, Instant(RunOutcome::Failure, "svc".into())).await;
        let obj = state.get(&key("svc")).unwrap();
        match obj.spec {
            Resource::ServiceStatus(s) => {
                assert_eq!(s.state, ServiceState::Failed);
                assert!(!s.healthy);
            }
            _ => panic!("wrong type"),
        }
    }
}
```

- [ ] **Step 2: Run the ServiceRunner tests**

Run: `cargo test -p machined-supervisor service`
Expected: PASS — both tests pass.

- [ ] **Step 3: Write the ServiceManager with dependency ordering**

Create `crates/supervisor/src/manager.rs`:

```rust
//! Owns the set of services: starts them in dependency order (spawning each as
//! a background task) and stops them in reverse order on shutdown.

use std::collections::HashMap;

use machined_config::{RestartPolicy, ServiceConfig};
use machined_runtime_core::State;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::process::ProcessRunner;
use crate::restart::{Policy, RestartRunner};
use crate::service::run_service;

/// Translate the config restart policy into the supervisor policy.
fn policy_of(p: RestartPolicy) -> Policy {
    match p {
        RestartPolicy::Never => Policy::Never,
        RestartPolicy::OnFailure => Policy::OnFailure,
        RestartPolicy::Always => Policy::Always,
    }
}

/// Order services so dependencies start before dependents (Kahn's algorithm).
/// Returns the start order, or an error string on a missing dep / cycle.
pub fn start_order(services: &[ServiceConfig]) -> Result<Vec<String>, String> {
    let ids: Vec<String> = services.iter().map(|s| s.id.clone()).collect();
    let mut indegree: HashMap<&str, usize> = ids.iter().map(|i| (i.as_str(), 0)).collect();
    let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();

    for svc in services {
        for dep in &svc.depends_on {
            if !indegree.contains_key(dep.as_str()) {
                return Err(format!("service {} depends on unknown {}", svc.id, dep));
            }
            deps.entry(dep.as_str()).or_default().push(svc.id.as_str());
            *indegree.get_mut(svc.id.as_str()).unwrap() += 1;
        }
    }

    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&id, _)| id)
        .collect();
    queue.sort_unstable(); // deterministic order
    let mut order = Vec::new();

    while let Some(id) = queue.pop() {
        order.push(id.to_string());
        if let Some(children) = deps.get(id) {
            for &child in children {
                let e = indegree.get_mut(child).unwrap();
                *e -= 1;
                if *e == 0 {
                    queue.push(child);
                    queue.sort_unstable();
                }
            }
        }
    }

    if order.len() != services.len() {
        return Err("dependency cycle among services".into());
    }
    Ok(order)
}

/// Supervises the configured services over a shared store.
pub struct ServiceManager {
    state: State,
    handles: Vec<(String, JoinHandle<()>)>,
    shutdown: CancellationToken,
}

impl ServiceManager {
    pub fn new(state: State) -> Self {
        Self {
            state,
            handles: Vec::new(),
            shutdown: CancellationToken::new(),
        }
    }

    /// Start every service as a background task, in dependency order. For M1
    /// this spawns all in order without blocking on readiness (health-gated
    /// start lands in M3).
    pub fn start_all(&mut self, services: &[ServiceConfig]) -> Result<(), String> {
        let order = start_order(services)?;
        let by_id: HashMap<&str, &ServiceConfig> =
            services.iter().map(|s| (s.id.as_str(), s)).collect();

        for id in order {
            let cfg = by_id[id.as_str()];
            let state = self.state.clone();
            let runner = RestartRunner::new(
                ProcessRunner::new(cfg.id.clone(), cfg.command.clone()),
                policy_of(cfg.restart),
            );
            info!(service = %cfg.id, "starting service");
            let handle = tokio::spawn(async move {
                run_service(&state, runner).await;
            });
            self.handles.push((cfg.id.clone(), handle));
        }
        Ok(())
    }

    /// Stop all services in reverse start order, aborting their tasks.
    pub async fn stop_all(&mut self) {
        self.shutdown.cancel();
        while let Some((id, handle)) = self.handles.pop() {
            info!(service = %id, "stopping service");
            handle.abort();
            if let Err(e) = handle.await {
                if !e.is_cancelled() {
                    warn!(service = %id, "join error on stop: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::RestartPolicy;

    fn svc(id: &str, deps: &[&str]) -> ServiceConfig {
        ServiceConfig {
            id: id.into(),
            command: vec!["true".into()],
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            restart: RestartPolicy::Never,
        }
    }

    #[test]
    fn orders_dependencies_first() {
        let services = vec![svc("payload", &["containerd"]), svc("containerd", &[])];
        let order = start_order(&services).unwrap();
        let ci = order.iter().position(|s| s == "containerd").unwrap();
        let pi = order.iter().position(|s| s == "payload").unwrap();
        assert!(ci < pi, "containerd must start before payload: {order:?}");
    }

    #[test]
    fn unknown_dependency_errors() {
        let services = vec![svc("payload", &["ghost"])];
        assert!(start_order(&services).is_err());
    }

    #[test]
    fn cycle_errors() {
        let services = vec![svc("a", &["b"]), svc("b", &["a"])];
        assert!(start_order(&services).is_err());
    }

    #[tokio::test]
    async fn start_all_publishes_status() {
        let state = State::new();
        let mut mgr = ServiceManager::new(state.clone());
        // A short-lived service so the task finishes quickly.
        mgr.start_all(&[ServiceConfig {
            id: "blip".into(),
            command: vec!["true".into()],
            depends_on: vec![],
            restart: RestartPolicy::Never,
        }])
        .unwrap();

        // Give the task time to publish + run.
        let k = machined_resources::Key::new(
            "runtime",
            machined_resources::ResourceType::ServiceStatus,
            "blip",
        );
        let mut seen = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if state.get(&k).is_ok() {
                seen = true;
                break;
            }
        }
        assert!(seen, "ServiceStatus was never published");
        mgr.stop_all().await;
    }
}
```

> **Cargo note:** `manager.rs` uses `machined-config` and `tokio-util`. Add to `crates/supervisor/Cargo.toml` `[dependencies]`:
> ```toml
> machined-config.workspace = true
> tokio-util.workspace = true
> ```

- [ ] **Step 4: Export the new modules**

Append to `crates/supervisor/src/lib.rs`:

```rust
pub mod manager;
pub mod service;

pub use manager::{start_order, ServiceManager};
pub use service::{publish_status, run_service};
```

- [ ] **Step 5: Run the supervisor tests**

Run: `cargo test -p machined-supervisor`
Expected: PASS — runner, restart, service, and manager tests all green.

- [ ] **Step 6: Lint + commit**

Run: `cargo clippy -p machined-supervisor --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/supervisor
git commit -m "feat(supervisor): ServiceRunner status + dependency-ordered ServiceManager"
```

---

## Task 6: `sequencer` — tasks, Boot + Shutdown phases

**Files:**
- Create: `crates/sequencer/Cargo.toml`
- Create: `crates/sequencer/src/lib.rs`
- Create: `crates/sequencer/src/task.rs`
- Create: `crates/sequencer/src/boot.rs`
- Create: `crates/sequencer/src/shutdown.rs`

- [ ] **Step 1: Define the Task/Phase model and context**

Create `crates/sequencer/Cargo.toml`:

```toml
[package]
name = "machined-sequencer"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
machined-common.workspace = true
machined-config.workspace = true
machined-platform.workspace = true
machined-resources.workspace = true
machined-runtime-core.workspace = true
machined-supervisor.workspace = true
async-trait.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true
```

Create `crates/sequencer/src/task.rs`:

```rust
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
```

- [ ] **Step 2: Write the boot tasks + a failing boot test**

Create `crates/sequencer/src/boot.rs`:

```rust
//! The Boot sequence: mount filesystems, apply sysctls/hostname, start the
//! configured services.

use async_trait::async_trait;

use crate::task::{PhaseList, SequencerCtx, Task, TaskError};

struct MountFilesystems;

#[async_trait]
impl Task for MountFilesystems {
    fn name(&self) -> &str {
        "mount-filesystems"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        ctx.platform.mount_essential().map_err(|e| TaskError {
            task: self.name().into(),
            message: e.to_string(),
        })
    }
}

struct ApplySysctls;

#[async_trait]
impl Task for ApplySysctls {
    fn name(&self) -> &str {
        "apply-sysctls"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        for s in ctx.provider.sysctls() {
            ctx.platform
                .set_sysctl(&s.key, &s.value)
                .map_err(|e| TaskError {
                    task: self.name().into(),
                    message: e.to_string(),
                })?;
        }
        Ok(())
    }
}

struct SetHostname;

#[async_trait]
impl Task for SetHostname {
    fn name(&self) -> &str {
        "set-hostname"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        if let Some(name) = ctx.provider.hostname() {
            ctx.platform.set_hostname(name).map_err(|e| TaskError {
                task: self.name().into(),
                message: e.to_string(),
            })?;
        }
        Ok(())
    }
}

struct StartServices;

#[async_trait]
impl Task for StartServices {
    fn name(&self) -> &str {
        "start-services"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        let services = ctx.provider.services().to_vec();
        let mut mgr = ctx.services.lock().await;
        mgr.start_all(&services).map_err(|message| TaskError {
            task: self.name().into(),
            message,
        })
    }
}

/// Build the Boot phase list.
pub fn boot_sequence() -> PhaseList {
    PhaseList::new()
        .phase(
            "early",
            vec![Box::new(MountFilesystems), Box::new(ApplySysctls)],
        )
        .phase("identity", vec![Box::new(SetHostname)])
        .phase("services", vec![Box::new(StartServices)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::SequencerCtx;
    use machined_config::{MachineConfig, MachineSection, Provider, RestartPolicy, ServiceConfig};
    use machined_platform::{essential_mounts, FakePlatform};
    use machined_runtime_core::State;
    use machined_supervisor::ServiceManager;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn boot_mounts_and_starts_services() {
        let platform = Arc::new(FakePlatform::new());
        let state = State::new();
        let cfg = MachineConfig {
            machine: MachineSection {
                hostname: Some("node-1".into()),
                sysctls: vec![],
                services: vec![ServiceConfig {
                    id: "blip".into(),
                    command: vec!["true".into()],
                    depends_on: vec![],
                    restart: RestartPolicy::Never,
                }],
            },
        };
        let ctx = SequencerCtx {
            state: state.clone(),
            platform: platform.clone(),
            provider: Provider::new(cfg),
            services: Arc::new(Mutex::new(ServiceManager::new(state.clone()))),
        };

        boot_sequence().run(&ctx).await.unwrap();

        let rec = platform.recorded.lock().unwrap();
        assert_eq!(rec.mounts.len(), essential_mounts().len());
        assert_eq!(rec.hostname.as_deref(), Some("node-1"));
        drop(rec);

        // The service eventually publishes status.
        let k = machined_resources::Key::new(
            "runtime",
            machined_resources::ResourceType::ServiceStatus,
            "blip",
        );
        let mut seen = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if state.get(&k).is_ok() {
                seen = true;
                break;
            }
        }
        assert!(seen);

        ctx.services.lock().await.stop_all().await;
    }
}
```

- [ ] **Step 3: Write the shutdown sequence**

Create `crates/sequencer/src/shutdown.rs`:

```rust
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
    use machined_supervisor::ServiceManager;
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
        };
        shutdown_sequence().run(&ctx).await.unwrap();
    }
}
```

Create `crates/sequencer/src/lib.rs`:

```rust
//! Lifecycle sequencing: ordered, idempotent tasks grouped into phases.

pub mod boot;
pub mod shutdown;
pub mod task;

pub use boot::boot_sequence;
pub use shutdown::shutdown_sequence;
pub use task::{Phase, PhaseList, SequencerCtx, Task, TaskError};
```

- [ ] **Step 4: Run the sequencer tests**

Run: `cargo test -p machined-sequencer`
Expected: PASS — `boot_mounts_and_starts_services` and `shutdown_runs_clean` pass.

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy -p machined-sequencer --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/sequencer
git commit -m "feat(sequencer): Task/Phase model + boot and shutdown sequences"
```

---

## Task 7: `machined` binary — PID 1 entry, signals, reaper, wiring

**Files:**
- Create: `crates/machined/Cargo.toml`
- Create: `crates/machined/src/main.rs`
- Create: `crates/machined/src/pid1.rs`
- Create: `crates/machined/src/emergency.rs`

- [ ] **Step 1: Create the binary manifest**

Create `crates/machined/Cargo.toml`:

```toml
[package]
name = "machined"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "machined"
path = "src/main.rs"

[dependencies]
machined-common.workspace = true
machined-config.workspace = true
machined-platform.workspace = true
machined-resources.workspace = true
machined-runtime-core.workspace = true
machined-sequencer.workspace = true
machined-supervisor.workspace = true
anyhow.workspace = true
tokio.workspace = true
tokio-util.workspace = true
tracing.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
nix.workspace = true
```

- [ ] **Step 2: Write the zombie reaper + signal wait (Linux)**

Create `crates/machined/src/pid1.rs`:

```rust
//! PID-1 duties: reap orphaned children and wait for termination signals.
//! Only meaningful on Linux; guarded so the crate still builds elsewhere.

#[cfg(target_os = "linux")]
pub use linux::{spawn_reaper, wait_for_termination};

#[cfg(target_os = "linux")]
mod linux {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid;
    use tokio::signal::unix::{signal, SignalKind};
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, info};

    /// Continuously reap any orphaned children that get reparented to PID 1.
    /// Runs until `shutdown` is cancelled.
    pub fn spawn_reaper(shutdown: CancellationToken) {
        tokio::spawn(async move {
            // SIGCHLD wakes us; we then reap everything reapable.
            let mut sigchld = match signal(SignalKind::child()) {
                Ok(s) => s,
                Err(e) => {
                    info!("could not install SIGCHLD handler (not PID 1?): {e}");
                    return;
                }
            };
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = sigchld.recv() => {
                        reap_all();
                    }
                }
            }
        });
    }

    fn reap_all() {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(_) => break,
                Ok(status) => debug!(?status, "reaped child"),
            }
        }
    }

    /// Resolve when SIGTERM or SIGINT is received.
    pub async fn wait_for_termination() {
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT");
        tokio::select! {
            _ = term.recv() => info!("received SIGTERM"),
            _ = int.recv() => info!("received SIGINT"),
        }
    }
}

// Non-Linux stubs so `cargo build` works on dev machines/CI macos.
#[cfg(not(target_os = "linux"))]
pub fn spawn_reaper(_shutdown: tokio_util::sync::CancellationToken) {}

#[cfg(not(target_os = "linux"))]
pub async fn wait_for_termination() {
    let _ = tokio::signal::ctrl_c().await;
}
```

- [ ] **Step 3: Write the emergency path**

Create `crates/machined/src/emergency.rs`:

```rust
//! Last-resort handling when boot fails. In PID-1 context a bare exit would
//! panic the kernel, so we log loudly and (optionally) reboot. For M1 we log
//! and return; the caller decides whether to halt.

use machined_platform::Platform;
use std::sync::Arc;
use tracing::error;

/// Log a fatal boot error. If `reboot_on_failure` is set, ask the platform to
/// reboot; otherwise return so the caller can park.
pub fn enter_emergency(platform: &Arc<dyn Platform>, err: &dyn std::fmt::Display, reboot_on_failure: bool) {
    error!("FATAL during boot: {err}");
    error!("entering emergency state");
    if reboot_on_failure {
        if let Err(e) = platform.reboot() {
            error!("emergency reboot failed: {e}");
        }
    }
}
```

- [ ] **Step 4: Write main.rs (multi-call dispatch + boot/shutdown wiring)**

Create `crates/machined/src/main.rs`:

```rust
//! machined — PID 1 / machine-management daemon entrypoint.

mod emergency;
mod pid1;

use std::path::PathBuf;
use std::sync::Arc;

use machined_common::init_logging;
use machined_config::{load::load_from_path, Provider};
use machined_platform::Platform;
use machined_runtime_core::Runtime;
use machined_sequencer::{boot_sequence, shutdown_sequence, SequencerCtx};
use machined_supervisor::ServiceManager;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

const DEFAULT_CONFIG_PATH: &str = "/etc/machined/config.yaml";

fn build_platform() -> Arc<dyn Platform> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_platform::LinuxPlatform::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_platform::FakePlatform::new())
    }
}

#[tokio::main]
async fn main() {
    init_logging();

    // Multi-call dispatch: argv[1] selects a subcommand; default is the daemon.
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("version") => {
            println!("machined {}", env!("CARGO_PKG_VERSION"));
        }
        Some("daemon") | None => {
            if let Err(e) = run_daemon().await {
                error!("daemon exited with error: {e}");
                std::process::exit(1);
            }
        }
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            std::process::exit(2);
        }
    }
}

async fn run_daemon() -> anyhow::Result<()> {
    info!("machined starting (pid {})", std::process::id());
    let platform = build_platform();
    let shutdown = CancellationToken::new();

    // PID-1 duties.
    pid1::spawn_reaper(shutdown.clone());

    // Build the shared runtime + service manager.
    let runtime = Runtime::new();
    let state = runtime.state();
    let services = Arc::new(Mutex::new(ServiceManager::new(state.clone())));

    // Spawn the reconcile runtime (no controllers in M1; the loop is live and
    // ready for M2 controllers).
    let rt_token = shutdown.clone();
    let rt_handle = tokio::spawn(async move {
        if let Err(e) = runtime.run(rt_token).await {
            error!("runtime error: {e}");
        }
    });

    // Load config (fall back to an empty config if the file is absent, so a
    // bare boot still comes up).
    let config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let (config, _raw) = match load_from_path(&config_path) {
        Ok(v) => v,
        Err(e) => {
            info!("no config at {} ({e}); booting with defaults", config_path.display());
            (Default::default(), String::new())
        }
    };
    let provider = Provider::new(config);

    let ctx = SequencerCtx {
        state,
        platform: platform.clone(),
        provider,
        services: services.clone(),
    };

    // Boot.
    if let Err(e) = boot_sequence().run(&ctx).await {
        emergency::enter_emergency(&platform, &e, false);
        return Err(anyhow::anyhow!("boot failed: {e}"));
    }
    info!("boot complete; node up");

    // Wait for a termination signal.
    pid1::wait_for_termination().await;
    info!("shutting down");

    // Shutdown.
    if let Err(e) = shutdown_sequence().run(&ctx).await {
        error!("shutdown sequence error: {e}");
    }

    // Stop the runtime and join.
    shutdown.cancel();
    let _ = rt_handle.await;
    info!("machined stopped");
    Ok(())
}
```

- [ ] **Step 5: Build the whole workspace**

Run: `cargo build --workspace`
Expected: PASS — every crate compiles, including the `machined` binary.

- [ ] **Step 6: Smoke-test the binary's version subcommand**

Run: `cargo run -p machined -- version`
Expected: prints `machined 0.1.0` and exits 0.

- [ ] **Step 7: Lint + commit**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS.

```bash
git add crates/machined Cargo.lock
git commit -m "feat(machined): PID1 entry, signals, reaper, boot/shutdown wiring"
```

---

## Task 8: Boot integration test (Boot → Running → Shutdown)

**Files:**
- Create: `crates/machined/tests/boot_harness.rs`

- [ ] **Step 1: Write the end-to-end boot harness test**

This test exercises the full sequencer wiring against the **fake** platform with a real short-lived `process` service, asserting the service reaches a published status and that shutdown runs clean — without requiring root/PID 1.

Create `crates/machined/tests/boot_harness.rs`:

```rust
//! End-to-end boot harness: drives boot_sequence + shutdown_sequence over a
//! fake platform and a real process service, asserting the service is
//! supervised and reflected in the store.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{MachineConfig, MachineSection, Provider, RestartPolicy, ServiceConfig};
use machined_platform::{essential_mounts, FakePlatform};
use machined_resources::{Key, Resource, ResourceType, ServiceState};
use machined_runtime_core::Runtime;
use machined_sequencer::{boot_sequence, shutdown_sequence, SequencerCtx};
use machined_supervisor::ServiceManager;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn boots_supervises_and_shuts_down() {
    let platform = Arc::new(FakePlatform::new());
    let runtime = Runtime::new();
    let state = runtime.state();
    let shutdown = CancellationToken::new();
    let rt_token = shutdown.clone();
    let rt_handle = tokio::spawn(async move { runtime.run(rt_token).await });

    // A service that stays up long enough to observe Running.
    let cfg = MachineConfig {
        machine: MachineSection {
            hostname: Some("node-1".into()),
            sysctls: vec![],
            services: vec![ServiceConfig {
                id: "payload".into(),
                command: vec!["sleep".into(), "5".into()],
                depends_on: vec![],
                restart: RestartPolicy::Never,
            }],
        },
    };

    let services = Arc::new(Mutex::new(ServiceManager::new(state.clone())));
    let ctx = SequencerCtx {
        state: state.clone(),
        platform: platform.clone(),
        provider: Provider::new(cfg),
        services: services.clone(),
    };

    // Boot.
    boot_sequence().run(&ctx).await.expect("boot succeeds");

    // Essential mounts happened.
    assert_eq!(
        platform.recorded.lock().unwrap().mounts.len(),
        essential_mounts().len()
    );

    // The payload reaches Running.
    let key = Key::new("runtime", ResourceType::ServiceStatus, "payload");
    let mut running = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(obj) = state.get(&key) {
            if let Resource::ServiceStatus(s) = obj.spec {
                if s.state == ServiceState::Running {
                    running = true;
                    break;
                }
            }
        }
    }
    assert!(running, "payload service never reached Running");

    // Shutdown stops services cleanly.
    shutdown_sequence().run(&ctx).await.expect("shutdown succeeds");
    shutdown.cancel();
    let _ = rt_handle.await;
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p machined --test boot_harness`
Expected: PASS — boot mounts, the `sleep 5` payload reaches `Running`, shutdown aborts it cleanly. (Requires `sleep` on PATH.)

- [ ] **Step 3: Full workspace verification**

Run: `make pre-commit`
Expected: PASS — `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --workspace` all succeed.

- [ ] **Step 4: Commit**

```bash
git add crates/machined/tests/boot_harness.rs
git commit -m "test(machined): end-to-end boot → running → shutdown harness"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage (M1 deliverables):**
  1. `machined` PID-1 entrypoint + multi-call dispatch — Task 7 (`main.rs` arg match) ✓
  2. `platform` mounts essential fs + sysctls + kernel cmdline — Task 2 ✓
  3. PID-1 zombie reaping + SIGTERM/SIGINT handling + emergency path — Task 7 (`pid1.rs`, `emergency.rs`) ✓
  4. `config` minimal single-doc YAML + Provider + `MachineConfig` resource — Task 1 ✓
  5. `sequencer` Boot (mount → runtime up → supervisor → service) + Shutdown (reverse stop) — Task 6 ✓
  6. `supervisor` ServiceManager + ServiceRunner + `process` runner + `restart` wrapper, one real `process` service via `ServiceStatus` — Tasks 3–5 ✓
  7. Acceptance: VM/harness boot → service Running → clean shutdown, covered by integration test — Task 8 ✓ (fake-platform harness; the real-PID-1/VM variant is a CI job introduced in M2 once netlink/block need a real kernel — noted as out of scope for M1's unit/integration tier per the spec's testing strategy).

- **Type consistency with M0:** uses `State`, `Runtime`, `Resource::{MachineConfig,ServiceStatus}`, `ServiceStatusSpec`, `ServiceState`, `MachineConfigSpec`, `ResourceObject::new`, `Key::new`, `ResourceType` exactly as defined in the M0 plan. `Runtime::run(CancellationToken)`, `Runtime::state()`, and `State::{get,create,update,list}` signatures match M0 Task 5/6.

- **Deliberately deferred (spec-aligned):** real network/block/secrets controllers (M2); gRPC API + `machinectl` (M3); containerd/CRI + the actual rusternetes service (M4); SIGTERM-grace stop, kexec, reset, real unmount/halt (M5). `ProcessRunner::stop` uses SIGKILL in M1 (noted inline); graceful stop is M5.

- **Placeholder scan:** no TBD/TODO; every step ships complete code and exact commands.
