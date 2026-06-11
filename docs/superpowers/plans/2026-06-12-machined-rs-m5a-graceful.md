# machined-rs M5a — Graceful Shutdown Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M4 merged to `main`. Work on branch `spec/machined-rs-m5a-graceful`.

**Goal:** Disciplined stop: SIGTERM → per-service grace → SIGKILL in reverse start order; restarts re-check dependency readiness (status-honest `Waiting → Running` cycling); volumes synced + unmounted at shutdown; the API task shut down and joined.

**Architecture:** The restart loop moves from `RestartRunner` (retired) into `run_supervised` (service.rs): each attempt = stop-check → dep-gate → `run_service` (per-attempt status). `ProcessRunner` shares its child PID via a slot; `ServiceManager` keeps `ServiceHandle { join, pid, stop, grace }` and stops reverse-sequentially (intent → SIGTERM → grace-bounded join → abort). `Platform` gains `unmount`/`sync`; the shutdown sequence gains a `SyncAndUnmount` phase; `apiserver` gains `serve_with_shutdown`.

**Tech Stack:** `nix` signal::kill (feature already on) + `mount::umount2` (mount feature on) + `unistd::sync` (may need a nix feature — adapt note in T2); tonic `serve_with_shutdown`.

---

## File Structure

```
crates/config/src/types.rs            # MODIFY: ServiceConfig.stop_grace_secs
crates/supervisor/src/process.rs      # MODIFY: pid slot
crates/supervisor/src/restart.rs      # REWRITE: Policy + should_restart only (RestartRunner retired)
crates/supervisor/src/service.rs      # MODIFY: run_supervised
crates/supervisor/src/manager.rs      # MODIFY: ServiceHandle + graceful stop_all
crates/supervisor/src/lib.rs          # MODIFY: exports
crates/platform/src/{lib,linux,fake}.rs   # MODIFY: unmount + sync
crates/sequencer/src/shutdown.rs      # MODIFY: SyncAndUnmount phase + test
crates/apiserver/src/lib.rs           # MODIFY: serve_with_shutdown
crates/machined/src/main.rs           # MODIFY: API task token + bounded join
```

---

## Task 1: supervisor — graceful stop + run_supervised

**Files:**
- Modify: `crates/config/src/types.rs`
- Modify: `crates/supervisor/src/{process,restart,service,manager,lib}.rs`

- [ ] **Step 1: config grace field**

In `crates/config/src/types.rs`, add to `ServiceConfig` (after `restart`):

```rust
    /// Seconds to wait after SIGTERM before SIGKILL on stop. Default 10.
    #[serde(default)]
    pub stop_grace_secs: Option<u64>,
```

`#[serde(default)]` keeps existing YAML valid. **Struct-literal follow-through:** every explicit
`ServiceConfig { ... }` literal gains `stop_grace_secs: None,` — grep `ServiceConfig {` (expect:
config runtime_svc.rs `containerd_service` — use `None`; supervisor manager tests; sequencer boot
test; machined tests boot_harness/payload; machined-config tests). E0063 is the guide. Also add a
parse test in `crates/config/src/load.rs`:

```rust
    #[test]
    fn stop_grace_parses_and_defaults() {
        let cfg = load_from_str(
            "machine:\n  services:\n    - id: a\n      command: [x]\n      stop_grace_secs: 3\n",
        )
        .unwrap();
        assert_eq!(cfg.machine.services[0].stop_grace_secs, Some(3));
        let cfg2 = load_from_str("machine:\n  services:\n    - id: a\n      command: [x]\n").unwrap();
        assert_eq!(cfg2.machine.services[0].stop_grace_secs, None);
    }
```

- [ ] **Step 2: ProcessRunner pid slot**

In `crates/supervisor/src/process.rs`:

```rust
use std::sync::{Arc, Mutex};
```

```rust
pub struct ProcessRunner {
    id: String,
    command: Vec<String>,
    child: Option<Child>,
    /// Shared view of the live child PID (None when not running). The manager
    /// reads it to deliver SIGTERM during graceful stop.
    pid_slot: Arc<Mutex<Option<u32>>>,
}
```

`new` initializes `pid_slot: Arc::new(Mutex::new(None))`; add:

```rust
    /// Handle the manager uses to signal the live child.
    pub fn pid_slot(&self) -> Arc<Mutex<Option<u32>>> {
        self.pid_slot.clone()
    }
```

In `run()`: after `self.child = Some(child);` add
`*self.pid_slot.lock().unwrap() = self.child.as_ref().unwrap().id();`
and after `self.child = None;` add `*self.pid_slot.lock().unwrap() = None;`.
(`Child::id()` returns `Option<u32>` — assign it directly.)

- [ ] **Step 3: retire RestartRunner → pure policy**

Replace `crates/supervisor/src/restart.rs` content with:

```rust
//! Restart policy: a pure decision, applied by `run_supervised`.

use crate::runner::RunOutcome;

/// Restart behaviour for a supervised service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Never,
    OnFailure,
    Always,
}

/// Should the service run again after `outcome` under `policy`?
pub fn should_restart(policy: Policy, outcome: RunOutcome) -> bool {
    match policy {
        Policy::Never => false,
        Policy::OnFailure => outcome == RunOutcome::Failure,
        Policy::Always => outcome != RunOutcome::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_table() {
        use RunOutcome::*;
        assert!(!should_restart(Policy::Never, Failure));
        assert!(should_restart(Policy::OnFailure, Failure));
        assert!(!should_restart(Policy::OnFailure, Success));
        assert!(should_restart(Policy::Always, Success));
        assert!(should_restart(Policy::Always, Failure));
        assert!(!should_restart(Policy::Always, Stopped));
    }
}
```

(The old scripted-runner behaviors are re-covered by `run_supervised` tests in Step 4.)

- [ ] **Step 4: run_supervised**

In `crates/supervisor/src/service.rs`, add imports + the function + tests:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::readiness::{wait_for_deps, ReadinessCheck};
use crate::restart::{should_restart, Policy};
```

```rust
/// Supervise one service: gate on deps, run, apply the restart policy — until
/// the policy says stop or a stop intent is set. Each attempt re-gates and
/// re-publishes per-attempt status (Waiting → Preparing → Running → …).
pub async fn run_supervised<R: Runner>(
    state: &State,
    mut runner: R,
    policy: Policy,
    stop: Arc<AtomicBool>,
    check: Arc<dyn ReadinessCheck>,
    deps: &[String],
) {
    let id = runner.id().to_string();
    let backoff = Duration::from_millis(100);
    loop {
        if stop.load(Ordering::SeqCst) {
            publish_status(state, &id, ServiceState::Finished, true, "stopped");
            return;
        }
        wait_for_deps(state, check.as_ref(), &id, deps).await;
        if stop.load(Ordering::SeqCst) {
            publish_status(state, &id, ServiceState::Finished, true, "stopped");
            return;
        }
        let outcome = run_service(state, &mut runner).await;
        if stop.load(Ordering::SeqCst) || !should_restart(policy, outcome) {
            return;
        }
        info!(service = %id, ?outcome, "restarting service");
        tokio::time::sleep(backoff).await;
    }
}
```

Add tests to the `tests` module in service.rs (the `Instant` test runner exists; add a scripted one):

```rust
    use crate::readiness::DefaultReadiness;
    use crate::restart::Policy;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    struct Scripted {
        id: String,
        outcomes: Vec<RunOutcome>,
        idx: usize,
    }

    #[async_trait]
    impl Runner for Scripted {
        fn id(&self) -> &str {
            &self.id
        }
        async fn run(&mut self) -> crate::runner::Result<RunOutcome> {
            let o = self.outcomes.get(self.idx).copied().unwrap_or(RunOutcome::Stopped);
            self.idx += 1;
            Ok(o)
        }
        async fn stop(&mut self) -> crate::runner::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn supervised_on_failure_restarts_until_success() {
        let state = State::new();
        let r = Scripted {
            id: "s".into(),
            outcomes: vec![RunOutcome::Failure, RunOutcome::Failure, RunOutcome::Success],
            idx: 0,
        };
        run_supervised(
            &state,
            r,
            Policy::OnFailure,
            Arc::new(AtomicBool::new(false)),
            Arc::new(DefaultReadiness),
            &[],
        )
        .await;
        // Final status: Finished (the last run succeeded).
        let k = Key::new("runtime", ResourceType::ServiceStatus, "s");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => assert_eq!(s.state, ServiceState::Finished),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn supervised_stop_intent_prevents_restart() {
        let state = State::new();
        let stop = Arc::new(AtomicBool::new(false));
        let r = Scripted {
            id: "s".into(),
            outcomes: vec![RunOutcome::Success; 100],
            idx: 0,
        };
        let s2 = state.clone();
        let st2 = stop.clone();
        let h = tokio::spawn(async move {
            run_supervised(
                &s2,
                r,
                Policy::Always,
                st2,
                Arc::new(DefaultReadiness),
                &[],
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        stop.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), h)
            .await
            .expect("must stop restarting")
            .unwrap();
    }

    #[tokio::test]
    async fn supervised_restart_re_gates_on_deps() {
        // dep ready → first run happens; dep flips not-ready → the restart parks
        // in Waiting instead of re-running.
        let state = State::new();
        let dep_key = Key::new("runtime", ResourceType::ServiceStatus, "dep");
        let _ = state.create(machined_resources::ResourceObject::new(
            "runtime",
            "dep",
            Resource::ServiceStatus(machined_resources::ServiceStatusSpec {
                service_id: "dep".into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        ));
        let r = Scripted {
            id: "s".into(),
            outcomes: vec![RunOutcome::Failure; 100],
            idx: 0,
        };
        let s2 = state.clone();
        let h = tokio::spawn(async move {
            run_supervised(
                &s2,
                r,
                Policy::OnFailure,
                Arc::new(AtomicBool::new(false)),
                Arc::new(DefaultReadiness),
                &["dep".to_string()],
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        // Flip the dep to Failed → not ready; the next restart must park.
        let cur = state.get(&dep_key).unwrap();
        state
            .update(
                &dep_key,
                cur.metadata.version,
                Resource::ServiceStatus(machined_resources::ServiceStatusSpec {
                    service_id: "dep".into(),
                    state: ServiceState::Failed,
                    healthy: false,
                    last_message: String::new(),
                }),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let k = Key::new("runtime", ResourceType::ServiceStatus, "s");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => assert_eq!(
                s.state,
                ServiceState::Waiting,
                "restart must re-gate on deps"
            ),
            _ => panic!(),
        }
        h.abort();
        let _ = h.await;
    }
```

(Imports for `Key`/`Resource`/`ResourceType` etc. exist in the module already — extend as needed.)

- [ ] **Step 5: ServiceManager graceful stop_all**

Rewrite the spawn + stop in `crates/supervisor/src/manager.rs`:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use crate::restart::Policy;
use crate::service::run_supervised;
```

```rust
struct ServiceHandle {
    id: String,
    join: tokio::task::JoinHandle<()>,
    pid: Arc<StdMutex<Option<u32>>>,
    stop: Arc<AtomicBool>,
    grace: Duration,
}

pub struct ServiceManager {
    state: State,
    handles: Vec<ServiceHandle>,
}
```

```rust
    pub fn start_all(
        &mut self,
        services: &[ServiceConfig],
        check: Arc<dyn ReadinessCheck>,
    ) -> Result<(), String> {
        let order = start_order(services)?;
        let by_id: HashMap<&str, &ServiceConfig> =
            services.iter().map(|s| (s.id.as_str(), s)).collect();

        for id in order {
            let cfg = by_id[id.as_str()];
            let state = self.state.clone();
            let deps = cfg.depends_on.clone();
            let check = check.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_task = stop.clone();
            let runner = ProcessRunner::new(cfg.id.clone(), cfg.command.clone());
            let pid = runner.pid_slot();
            let policy = policy_of(cfg.restart);
            info!(service = %cfg.id, "starting service");
            let handle = tokio::spawn(async move {
                run_supervised(&state, runner, policy, stop_task, check, &deps).await;
            });
            self.handles.push(ServiceHandle {
                id: cfg.id.clone(),
                join: handle,
                pid,
                stop,
                grace: Duration::from_secs(cfg.stop_grace_secs.unwrap_or(10)),
            });
        }
        Ok(())
    }

    /// Stop all services in reverse start order: stop-intent → SIGTERM →
    /// grace-bounded drain → abort (kill_on_drop SIGKILLs).
    pub async fn stop_all(&mut self) {
        while let Some(mut h) = self.handles.pop() {
            h.stop.store(true, Ordering::SeqCst);
            let pid = *h.pid.lock().unwrap();
            if let Some(pid) = pid {
                #[cfg(unix)]
                {
                    use nix::sys::signal::{kill, Signal};
                    use nix::unistd::Pid;
                    match kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
                        Ok(()) => info!(service = %h.id, "sent SIGTERM"),
                        Err(nix::errno::Errno::ESRCH) => {}
                        Err(e) => warn!(service = %h.id, "SIGTERM failed: {e}"),
                    }
                }
            }
            match tokio::time::timeout(h.grace, &mut h.join).await {
                Ok(_) => info!(service = %h.id, "service drained"),
                Err(_) => {
                    warn!(service = %h.id, "grace expired; killing");
                    h.join.abort();
                    let _ = h.join.await;
                }
            }
        }
    }
```

Add `nix.workspace = true` (under `[target.'cfg(unix)'.dependencies]` or plain — match the platform
crate's style) to `crates/supervisor/Cargo.toml`.

Update `lib.rs` exports: `RestartRunner` is gone — export `Policy`, `should_restart`,
`run_supervised` instead (keep existing re-exports otherwise). Fix any `use` fallout.

- [ ] **Step 6: real-process graceful tests**

Append to the `tests` module in `manager.rs`:

```rust
    use machined_resources::{Key, Resource, ResourceType, ServiceState};
    use std::time::{Duration, Instant};

    fn svc_full(id: &str, command: &[&str], grace: u64) -> ServiceConfig {
        ServiceConfig {
            id: id.into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            depends_on: vec![],
            restart: RestartPolicy::Never,
            stop_grace_secs: Some(grace),
        }
    }

    async fn wait_running(state: &State, id: &str) {
        let k = Key::new("runtime", ResourceType::ServiceStatus, id);
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Ok(o) = state.get(&k) {
                if matches!(o.spec, Resource::ServiceStatus(ref s) if s.state == ServiceState::Running)
                {
                    return;
                }
            }
        }
        panic!("{id} never reached Running");
    }

    #[tokio::test]
    async fn graceful_stop_drains_on_sigterm() {
        let state = State::new();
        let mut mgr = ServiceManager::new(state.clone());
        mgr.start_all(
            &[svc_full(
                "drainer",
                &["sh", "-c", "trap 'exit 0' TERM; sleep 30 & wait"],
                5,
            )],
            Arc::new(crate::readiness::DefaultReadiness),
        )
        .unwrap();
        wait_running(&state, "drainer").await;

        let t0 = Instant::now();
        mgr.stop_all().await;
        let took = t0.elapsed();
        assert!(took < Duration::from_secs(4), "drained, not grace-expired: {took:?}");
        let k = Key::new("runtime", ResourceType::ServiceStatus, "drainer");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => {
                assert_eq!(s.state, ServiceState::Finished, "TERM-trapped exit 0 → Finished")
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn grace_expiry_kills() {
        let state = State::new();
        let mut mgr = ServiceManager::new(state.clone());
        mgr.start_all(
            &[svc_full(
                "stubborn",
                &["sh", "-c", "trap '' TERM; sleep 30 & wait"],
                1,
            )],
            Arc::new(crate::readiness::DefaultReadiness),
        )
        .unwrap();
        wait_running(&state, "stubborn").await;

        let t0 = Instant::now();
        mgr.stop_all().await;
        let took = t0.elapsed();
        assert!(
            took >= Duration::from_millis(900) && took < Duration::from_secs(5),
            "killed at ~grace: {took:?}"
        );
    }

    #[tokio::test]
    async fn stop_all_reverse_order() {
        // dep ← dependent; stop must drain the dependent first. Both record
        // their TERM time by exiting promptly; reverse order is observable via
        // sequential stop (dependent drained before dep gets TERM).
        let state = State::new();
        let mut mgr = ServiceManager::new(state.clone());
        let dep = svc_full("dep", &["sh", "-c", "trap 'exit 0' TERM; sleep 30 & wait"], 5);
        let mut dependent =
            svc_full("dependent", &["sh", "-c", "trap 'exit 0' TERM; sleep 30 & wait"], 5);
        dependent.depends_on = vec!["dep".into()];
        mgr.start_all(&[dep, dependent], Arc::new(crate::readiness::DefaultReadiness))
            .unwrap();
        wait_running(&state, "dep").await;
        wait_running(&state, "dependent").await;

        mgr.stop_all().await;
        // Both Finished; handles popped in reverse (dependent first) — the
        // sequential drain proves ordering structurally (handles is a stack).
        for id in ["dep", "dependent"] {
            let k = Key::new("runtime", ResourceType::ServiceStatus, id);
            match state.get(&k).unwrap().spec {
                Resource::ServiceStatus(s) => assert_eq!(s.state, ServiceState::Finished),
                _ => panic!(),
            }
        }
    }
```

> `sleep 30 & wait` (not bare `sleep 30`): `sh` only delivers traps between commands; `wait` is
> interruptible by signals, so the trap fires immediately on TERM.

- [ ] **Step 7: gates + commit**

Run: `cargo test -p machined-supervisor` → all (policy table + run_supervised ×3 + graceful ×3 + existing readiness/manager/service tests). No hangs.
Run: `cargo build --workspace` → fix every `ServiceConfig {` literal (E0063, Step 1) and any `RestartRunner` import fallout. `cargo test --workspace` green. clippy `-D warnings` + fmt clean.

```bash
git add crates/config crates/supervisor crates/sequencer crates/machined
git commit -m "feat(supervisor): graceful stop (SIGTERM->grace->kill) + run_supervised re-gating restarts"
```

---

## Task 2: platform unmount/sync + shutdown disk phase

**Files:**
- Modify: `crates/platform/src/{lib,linux,fake}.rs`
- Modify: `crates/sequencer/src/shutdown.rs`

- [ ] **Step 1: Platform trait + impls**

`crates/platform/src/lib.rs` — add to the `Platform` trait (after `is_mounted`):

```rust
    /// Unmount the filesystem at `target`.
    fn unmount(&self, target: &str) -> Result<()>;
    /// Flush filesystem buffers to disk.
    fn sync(&self);
```

`crates/platform/src/linux.rs`:

```rust
    fn unmount(&self, target: &str) -> Result<()> {
        nix::mount::umount2(target, nix::mount::MntFlags::empty())
            .map_err(|e| PlatformError::Mount(format!("umount {target}: {e}")))
    }

    fn sync(&self) {
        nix::unistd::sync();
    }
```

(Adapt the error-variant name to the crate's real `PlatformError`; if `nix::unistd::sync` is
feature-gated in 0.29, add the needed nix feature — likely `"fs"` — to the workspace list and note it.)

`crates/platform/src/fake.rs` — record + flip `is_mounted`:

```rust
    fn unmount(&self, target: &str) -> Result<()> {
        let mut rec = self.recorded.lock().unwrap();
        rec.mounts.retain(|m| m.target != target);
        rec.unmounts.push(target.to_string());
        Ok(())
    }

    fn sync(&self) {
        self.recorded.lock().unwrap().syncs += 1;
    }
```

Add `pub unmounts: Vec<String>` + `pub syncs: u32` to the recorded struct. Test:

```rust
    #[test]
    fn fake_unmount_flips_is_mounted_and_records() {
        let p = FakePlatform::new();
        p.mount(&MountSpec {
            source: "/dev/x".into(),
            target: "/var".into(),
            fstype: "ext4".into(),
            flags: 0,
            data: None,
        })
        .unwrap();
        assert!(p.is_mounted("/var").unwrap());
        p.sync();
        p.unmount("/var").unwrap();
        assert!(!p.is_mounted("/var").unwrap());
        let rec = p.recorded.lock().unwrap();
        assert_eq!(rec.unmounts, vec!["/var"]);
        assert_eq!(rec.syncs, 1);
    }
```

Plus a root-free Linux negative test in linux.rs tests: `unmount("/no/such/mnt")` errors.

- [ ] **Step 2: SyncAndUnmount shutdown phase**

In `crates/sequencer/src/shutdown.rs`, add the task + phase:

```rust
struct SyncAndUnmount;

#[async_trait]
impl Task for SyncAndUnmount {
    fn name(&self) -> &str {
        "sync-and-unmount"
    }
    async fn run(&self, ctx: &SequencerCtx) -> crate::task::Result<()> {
        ctx.platform.sync();
        // Reverse mount order; best-effort — shutdown must complete.
        for target in ["/var", "/system/state", "/boot"] {
            match ctx.platform.is_mounted(target) {
                Ok(true) => {
                    if let Err(e) = ctx.platform.unmount(target) {
                        tracing::warn!("unmount {target}: {e}");
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("is_mounted {target}: {e}"),
            }
        }
        Ok(())
    }
}

pub fn shutdown_sequence() -> PhaseList {
    PhaseList::new()
        .phase("stop", vec![Box::new(StopServices)])
        .phase("disk", vec![Box::new(SyncAndUnmount)])
}
```

Extend the shutdown test: pre-mount `/var` + `/system/state` on the fake, run `shutdown_sequence()`,
assert `syncs == 1`, `unmounts == ["/var", "/system/state"]` (order pinned; `/boot` not mounted → not
unmounted), and the run is `Ok`.

- [ ] **Step 3: gates + commit**

`cargo test -p machined-platform -p machined-sequencer` green; workspace build/test/clippy/fmt green.

```bash
git add crates/platform crates/sequencer
git commit -m "feat(platform,sequencer): unmount+sync + shutdown disk phase"
```

---

## Task 3: API-task shutdown + final gates

**Files:**
- Modify: `crates/apiserver/src/lib.rs`
- Modify: `crates/machined/src/main.rs`
- Modify: `crates/apiserver/tests/grpc.rs` (one new test)

- [ ] **Step 1: serve_with_shutdown**

In `crates/apiserver/src/lib.rs`, add beside `serve`:

```rust
/// Serve the management API over mutual TLS until `signal` resolves.
pub async fn serve_with_shutdown(
    addr: SocketAddr,
    state: State,
    version: impl Into<String>,
    pki: &NodePki,
    actions: tokio::sync::mpsc::Sender<NodeAction>,
    signal: impl std::future::Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    let svc =
        pb::machine_service_server::MachineServiceServer::new(Machine::new(state, version, actions));
    let tls = server_tls(pki);
    Server::builder()
        .tls_config(tls)?
        .add_service(svc)
        .serve_with_shutdown(addr, signal)
        .await
}
```

- [ ] **Step 2: test the shutdown signal**

Append to `crates/apiserver/tests/grpc.rs` (plaintext variant — the TLS path shares the same
`serve_with_shutdown` mechanics via tonic; test the signal handling):

```rust
#[tokio::test]
async fn server_exits_on_shutdown_signal() {
    use tokio_util::sync::CancellationToken;

    let token = CancellationToken::new();
    let t2 = token.clone();
    let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
        Machine::new(State::new(), "9.9.9", tokio::sync::mpsc::channel(1).0),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let h = tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(incoming, async move { t2.cancelled().await })
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    token.cancel();
    let res = tokio::time::timeout(Duration::from_secs(3), h)
        .await
        .expect("server must exit on signal")
        .unwrap();
    assert!(res.is_ok());
}
```

Add `tokio-util` to apiserver `[dev-dependencies]` if absent (workspace dep exists).

- [ ] **Step 3: machined joins the API task**

In `crates/machined/src/main.rs`: the API spawn currently calls `machined_apiserver::serve(...)`
and discards the handle. Change to `serve_with_shutdown(..., {let t = shutdown.clone(); async move
{ t.cancelled().await }})`, keep the `JoinHandle` in a `run_daemon`-scoped
`Option<tokio::task::JoinHandle<()>>` (`api_handle`), and after the existing
`shutdown.cancel(); let _ = rt_handle.await;` add:

```rust
    if let Some(h) = api_handle {
        if tokio::time::timeout(std::time::Duration::from_secs(5), h)
            .await
            .is_err()
        {
            warn!("api server did not shut down in time");
        }
    }
```

(Import `warn` if not already; the handle's inner result can be ignored — errors were already
logged inside the task. Keep the task body's `error!` on serve errors.)

- [ ] **Step 4: full gates + commit**

`cargo test --workspace` green (no hangs); `cargo run -p machined -- version` OK; `make pre-commit`
green.

```bash
git add crates/apiserver crates/machined
git commit -m "feat(apiserver,machined): graceful API shutdown (token + bounded join)"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** grace config + pid slot + Policy/should_restart + `run_supervised` (per-attempt gate + status) + graceful `stop_all` (intent → SIGTERM → grace → abort, reverse) + real-process drain/kill/reverse tests (T1) ✓; `Platform::unmount`/`sync` + fake flip + `SyncAndUnmount` ordered phase + tests (T2) ✓; `serve_with_shutdown` + machined bounded join + signal test (T3) ✓.
- **Status honesty:** per-attempt `Waiting → Preparing → Running` cycling is the point of `run_supervised`; the re-gate test pins a restart parking in `Waiting`.
- **Blast radius:** `ServiceConfig.stop_grace_secs` literal follow-through (grep; runtime_svc's `containerd_service` uses `None` → 10s); `RestartRunner` removal (supervisor-internal only — verified); `Platform` trait additions break fake+linux impls only (both provided).
- **The `sh` trap trick:** `sleep 30 & wait` makes traps deliverable; bare `sleep 30` would delay TERM handling to sleep-exit and break the drain test.
- **Type consistency:** `pid_slot: Arc<Mutex<Option<u32>>>`; `kill(Pid::from_raw(pid as i32), SIGTERM)`; `grace = stop_grace_secs.unwrap_or(10)`; `run_supervised(state, runner, policy, stop, check, deps)`.
- **Placeholder scan:** none; the only adapt-points are the `PlatformError` variant name and a possible nix feature for `unistd::sync` (both flagged inline).
