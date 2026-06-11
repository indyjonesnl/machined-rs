# machined-rs M4b — Payload Bring-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M4a merged to `main`. Work on branch `spec/machined-rs-m4b-payload`.

**Goal:** Health-gated `depends_on`: a payload service starts only when its dependencies are genuinely ready (containerd's readiness = `RuntimeStatus.ready` from the CRI probe), observable via a new `ServiceState::Waiting`. Completes milestone M4.

**Architecture:** The supervisor gains an injected `ReadinessCheck` (trait); each spawned service task waits (200ms poll, indefinite, `Waiting` published) for its deps before running its `Runner`. `DefaultReadiness` = dep `Running && healthy` **or** `Finished` (run-once deps satisfy). machined's `RuntimeReadiness` adds the containerd↔`RuntimeStatus.ready` rule; `SequencerCtx` carries the check.

**Tech Stack:** existing crates only — no new dependencies.

---

## File Structure

```
crates/resources/src/resource.rs       # MODIFY: ServiceState::Waiting
crates/supervisor/src/readiness.rs     # NEW: ReadinessCheck, DefaultReadiness, wait_for_deps
crates/supervisor/src/manager.rs       # MODIFY: start_all(services, check) + task wait
crates/supervisor/src/lib.rs           # MODIFY: pub mod readiness + re-exports
crates/sequencer/src/task.rs           # MODIFY: SequencerCtx.readiness
crates/sequencer/src/boot.rs           # MODIFY: pass check to start_all (+ test fixture)
crates/sequencer/src/shutdown.rs       # MODIFY: test fixture (SequencerCtx literal)
crates/machined/src/main.rs            # MODIFY: RuntimeReadiness + wiring + unit tests
crates/machined/tests/boot_harness.rs  # MODIFY: SequencerCtx literal
crates/machined/tests/payload.rs       # NEW: hermetic flip e2e (M4 acceptance)
docs/examples/node-with-kubelet.yaml   # NEW: payload-agnostic example
README.md                              # MODIFY: example pointer (one paragraph)
```

---

## Task 1: supervisor readiness gate

**Files:**
- Modify: `crates/resources/src/resource.rs`
- Create: `crates/supervisor/src/readiness.rs`
- Modify: `crates/supervisor/src/manager.rs`
- Modify: `crates/supervisor/src/lib.rs`

- [ ] **Step 1: Add the Waiting state**

In `crates/resources/src/resource.rs`, add to `ServiceState` (after `Preparing`):

```rust
    /// Waiting for dependencies to become ready.
    Waiting,
```

Run `cargo build --workspace` — adding the variant must not break anything (no exhaustive `match` on
`ServiceState` exists; states render via `{:?}`). If E0004 appears anywhere, add the `Waiting` arm
there and note it.

- [ ] **Step 2: The readiness module**

Create `crates/supervisor/src/readiness.rs`:

```rust
//! Health-gated dependency readiness: when may a dependent service start?

use std::time::Duration;

use machined_resources::{Key, Resource, ResourceType, ServiceState};
use machined_runtime_core::State;
use tracing::warn;

use crate::service::publish_status;

/// Decides when a dependency service is ready to be depended on.
pub trait ReadinessCheck: Send + Sync {
    /// True iff `dep_id` is ready (its dependents may start).
    fn is_ready(&self, state: &State, dep_id: &str) -> bool;
}

/// Default rule: the dep's ServiceStatus is (Running && healthy) OR Finished —
/// a run-once dependency that completed successfully is satisfied. Anything
/// else (absent, Waiting, Preparing, Failed, Skipped, unhealthy) is not ready.
pub struct DefaultReadiness;

impl ReadinessCheck for DefaultReadiness {
    fn is_ready(&self, state: &State, dep_id: &str) -> bool {
        let key = Key::new("runtime", ResourceType::ServiceStatus, dep_id);
        match state.get(&key).map(|o| o.spec) {
            Ok(Resource::ServiceStatus(s)) => {
                (s.state == ServiceState::Running && s.healthy)
                    || s.state == ServiceState::Finished
            }
            _ => false,
        }
    }
}

/// Block until every dep is ready, publishing a Waiting status meanwhile.
/// Returns immediately when `deps` is empty or already ready.
pub async fn wait_for_deps(
    state: &State,
    check: &dyn ReadinessCheck,
    id: &str,
    deps: &[String],
) {
    let ready = |deps: &[String]| deps.iter().all(|d| check.is_ready(state, d));
    if deps.is_empty() || ready(deps) {
        return;
    }
    publish_status(
        state,
        id,
        ServiceState::Waiting,
        false,
        &format!("waiting for: {}", deps.join(",")),
    );
    let mut ticks: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if ready(deps) {
            return;
        }
        ticks += 1;
        if ticks % 150 == 0 {
            let pending: Vec<&String> =
                deps.iter().filter(|d| !check.is_ready(state, d)).collect();
            warn!(service = id, pending = ?pending, "still waiting for dependencies");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{ResourceObject, ServiceStatusSpec};

    fn put(state: &State, id: &str, st: ServiceState, healthy: bool) {
        let _ = state.create(ResourceObject::new(
            "runtime",
            id,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: id.into(),
                state: st,
                healthy,
                last_message: String::new(),
            }),
        ));
    }

    #[test]
    fn default_readiness_truth_table() {
        let state = State::new();
        let r = DefaultReadiness;
        assert!(!r.is_ready(&state, "absent"));

        put(&state, "running-healthy", ServiceState::Running, true);
        put(&state, "running-unhealthy", ServiceState::Running, false);
        put(&state, "finished", ServiceState::Finished, false);
        put(&state, "failed", ServiceState::Failed, false);
        put(&state, "waiting", ServiceState::Waiting, false);

        assert!(r.is_ready(&state, "running-healthy"));
        assert!(!r.is_ready(&state, "running-unhealthy"));
        assert!(r.is_ready(&state, "finished"), "run-once success satisfies");
        assert!(!r.is_ready(&state, "failed"));
        assert!(!r.is_ready(&state, "waiting"));
    }

    #[tokio::test]
    async fn wait_unblocks_when_dep_flips() {
        let state = State::new();
        put(&state, "dep", ServiceState::Preparing, false);

        let s2 = state.clone();
        let waiter = tokio::spawn(async move {
            wait_for_deps(&s2, &DefaultReadiness, "svc", &["dep".to_string()]).await;
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiter.is_finished(), "must still be waiting");

        // svc shows Waiting in the store.
        let k = Key::new("runtime", ResourceType::ServiceStatus, "svc");
        match state.get(&k).unwrap().spec {
            Resource::ServiceStatus(s) => assert_eq!(s.state, ServiceState::Waiting),
            _ => panic!("wrong type"),
        }

        // Flip the dep → the waiter finishes.
        let k = Key::new("runtime", ResourceType::ServiceStatus, "dep");
        let cur = state.get(&k).unwrap();
        state
            .update(
                &k,
                cur.metadata.version,
                Resource::ServiceStatus(ServiceStatusSpec {
                    service_id: "dep".into(),
                    state: ServiceState::Running,
                    healthy: true,
                    last_message: String::new(),
                }),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), waiter)
            .await
            .expect("waiter must unblock")
            .unwrap();
    }
}
```

- [ ] **Step 3: Thread the check through start_all**

In `crates/supervisor/src/manager.rs`:
- imports: add `use std::sync::Arc;` (if absent) and `use crate::readiness::{wait_for_deps, ReadinessCheck};`
- change the signature and the spawn body:

```rust
    /// Start every service as a background task, in dependency order. Each
    /// task first waits (publishing Waiting) until `check` reports all of its
    /// depends_on ready, then runs the service.
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
            let sid = cfg.id.clone();
            let runner = RestartRunner::new(
                ProcessRunner::new(cfg.id.clone(), cfg.command.clone()),
                policy_of(cfg.restart),
            );
            info!(service = %cfg.id, "starting service");
            let handle = tokio::spawn(async move {
                wait_for_deps(&state, check.as_ref(), &sid, &deps).await;
                run_service(&state, runner).await;
            });
            self.handles.push((cfg.id.clone(), handle));
        }
        Ok(())
    }
```

Also update the stale doc comment ("health-gated start lands in M3" — it lands here).

In `crates/supervisor/src/lib.rs`: `pub mod readiness;` + `pub use readiness::{DefaultReadiness, ReadinessCheck};`.

- [ ] **Step 4: Fix supervisor-internal call sites (CAREFUL: run-once dep hang)**

Every existing `start_all(&services)` call gains `, Arc::new(DefaultReadiness)`. In the
`manager.rs` tests: **inspect any test with a dependency chain.** Under health gating, a dependent
waits for its dep to be Running+healthy or Finished — a short-lived dep (`true`) reaches `Finished`,
which now satisfies. A dep that FAILS would hang its dependent: if a test dep chain relies on
start-order-only semantics with a failing dep, switch the dep's command to `["true"]` (Finished) or
a long-running `["sleep","5"]` (Running). Adapt minimally and report what changed.

- [ ] **Step 5: Test + commit**

Run: `cargo test -p machined-supervisor` → existing + `default_readiness_truth_table` + `wait_unblocks_when_dep_flips` pass, no hangs (if a test hangs >60s, a dep chain needs the Step 4 fix).
Run: `cargo build --workspace` → FAILS in sequencer/machined (start_all callers) — expected; Task 2 fixes them. If you must keep the workspace green per-commit, do Task 2's mechanical call-site fixes in this commit too (preferred: do them now, commit once green).
Run: `cargo clippy -p machined-supervisor --all-targets -- -D warnings` → clean. fmt clean.

> **Workspace-green rule:** do the minimal Task-2 call-site edits (sequencer boot.rs `start_all`
> caller + `SequencerCtx` field additions listed in Task 2 Steps 1–2) in THIS commit if the workspace
> would otherwise not build. The split below assumes you keep each commit green.

```bash
git add crates/resources crates/supervisor crates/sequencer crates/machined
git commit -m "feat(supervisor): health-gated depends_on via ReadinessCheck + Waiting state"
```

---

## Task 2: sequencer + machined wiring (RuntimeReadiness)

**Files:**
- Modify: `crates/sequencer/src/task.rs`
- Modify: `crates/sequencer/src/boot.rs`
- Modify: `crates/sequencer/src/shutdown.rs` (test fixture)
- Modify: `crates/machined/src/main.rs`
- Modify: `crates/machined/tests/boot_harness.rs`

- [ ] **Step 1: SequencerCtx carries the check**

In `crates/sequencer/src/task.rs`, add to `SequencerCtx`:

```rust
    pub readiness: Arc<dyn ReadinessCheck>,
```

with `use machined_supervisor::ReadinessCheck;` (sequencer already depends on supervisor).

- [ ] **Step 2: Boot passes it through + fixture updates**

In `crates/sequencer/src/boot.rs` StartServices:

```rust
        mgr.start_all(&services, ctx.readiness.clone())
```

Every `SequencerCtx { ... }` literal (boot.rs test, shutdown.rs test, machined/tests/boot_harness.rs,
machined/src/main.rs) gains `readiness: Arc::new(DefaultReadiness),` — except machined/src/main.rs
which uses `RuntimeReadiness` (Step 3). Import `machined_supervisor::DefaultReadiness` where needed.
Grep `SequencerCtx {` for stragglers; E0063 is the guide.

- [ ] **Step 3: RuntimeReadiness in machined**

In `crates/machined/src/main.rs`, add (near the other helpers):

```rust
/// Default service readiness, plus: the built-in containerd service is ready
/// only once the CRI probe reports the runtime ready (RuntimeStatus.ready).
struct RuntimeReadiness;

impl machined_supervisor::ReadinessCheck for RuntimeReadiness {
    fn is_ready(&self, state: &machined_runtime_core::State, dep_id: &str) -> bool {
        use machined_resources::{Key, Resource, ResourceType};
        let base = machined_supervisor::DefaultReadiness.is_ready(state, dep_id);
        if dep_id != machined_config::RUNTIME_SERVICE_ID {
            return base;
        }
        let cri_ready = matches!(
            state
                .get(&Key::new(NS_RUNTIME, ResourceType::RuntimeStatus, "containerd"))
                .map(|o| o.spec),
            Ok(Resource::RuntimeStatus(r)) if r.ready
        );
        base && cri_ready
    }
}

const NS_RUNTIME: &str = "runtime";
```

In `run_daemon`, the `SequencerCtx` literal gains:

```rust
        readiness: Arc::new(RuntimeReadiness),
```

Add unit tests at the bottom of main.rs (a `#[cfg(test)] mod tests` if none exists):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use machined_resources::{Resource, ResourceObject, RuntimeStatus, ServiceState, ServiceStatusSpec};
    use machined_runtime_core::State;
    use machined_supervisor::ReadinessCheck;

    fn svc_running(state: &State, id: &str) {
        let _ = state.create(ResourceObject::new(
            "runtime",
            id,
            Resource::ServiceStatus(ServiceStatusSpec {
                service_id: id.into(),
                state: ServiceState::Running,
                healthy: true,
                last_message: String::new(),
            }),
        ));
    }

    #[test]
    fn containerd_needs_cri_ready_too() {
        let state = State::new();
        svc_running(&state, "containerd");
        // Process running but CRI not ready → NOT ready.
        assert!(!RuntimeReadiness.is_ready(&state, "containerd"));

        let _ = state.create(ResourceObject::new(
            "runtime",
            "containerd",
            Resource::RuntimeStatus(RuntimeStatus {
                ready: true,
                name: "containerd".into(),
                version: "2".into(),
            }),
        ));
        assert!(RuntimeReadiness.is_ready(&state, "containerd"));
    }

    #[test]
    fn other_services_use_default_rule_only() {
        let state = State::new();
        svc_running(&state, "payload");
        // No RuntimeStatus anywhere — non-containerd ids don't need it.
        assert!(RuntimeReadiness.is_ready(&state, "payload"));
    }
}
```

- [ ] **Step 4: Build + test + commit**

Run: `cargo build --workspace` → PASS (all literals/callers fixed).
Run: `cargo test -p machined-sequencer -p machined --lib --bins` → sequencer tests + the 2 new machined unit tests pass.
Run: `cargo run -p machined -- version` → `machined 0.1.0`. clippy/fmt clean.

```bash
git add crates/sequencer crates/machined
git commit -m "feat(machined,sequencer): RuntimeReadiness gate (containerd needs CRI ready)"
```

---

## Task 3: the M4 acceptance e2e + docs example

**Files:**
- Create: `crates/machined/tests/payload.rs`
- Create: `docs/examples/node-with-kubelet.yaml`
- Modify: `README.md`

- [ ] **Step 1: The hermetic flip e2e**

Create `crates/machined/tests/payload.rs`:

```rust
//! M4 acceptance: a payload service with depends_on [containerd] stays Waiting
//! until the CRI probe reports the runtime ready, then starts. Hermetic: the
//! "containerd" stand-in is `sleep 30`; CRI is a fake that flips.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{RestartPolicy, ServiceConfig};
use machined_cri::{CriClient, CriError, RuntimeVersion};
use machined_resources::{Key, Resource, ResourceType, RuntimeStatus, ServiceState};
use machined_runtime_core::State;
use machined_supervisor::{DefaultReadiness, ReadinessCheck, ServiceManager};

/// The machined RuntimeReadiness rule, restated for the test (the binary's
/// private type isn't linkable from an integration test).
struct RuntimeReadiness;
impl ReadinessCheck for RuntimeReadiness {
    fn is_ready(&self, state: &State, dep_id: &str) -> bool {
        let base = DefaultReadiness.is_ready(state, dep_id);
        if dep_id != "containerd" {
            return base;
        }
        let cri = matches!(
            state
                .get(&Key::new("runtime", ResourceType::RuntimeStatus, "containerd"))
                .map(|o| o.spec),
            Ok(Resource::RuntimeStatus(r)) if r.ready
        );
        base && cri
    }
}

/// A flippable CRI fake (FakeCriClient's ready is fixed at construction).
struct FlipCri {
    ready: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl CriClient for FlipCri {
    async fn version(&self) -> Result<RuntimeVersion, CriError> {
        Ok(RuntimeVersion {
            runtime_name: "containerd".into(),
            runtime_version: "2.0.0".into(),
        })
    }
    async fn ready(&self) -> Result<bool, CriError> {
        Ok(self.ready.load(std::sync::atomic::Ordering::SeqCst))
    }
}

fn svc_state(state: &State, id: &str) -> Option<ServiceState> {
    match state
        .get(&Key::new("runtime", ResourceType::ServiceStatus, id))
        .ok()?
        .spec
    {
        Resource::ServiceStatus(s) => Some(s.state),
        _ => None,
    }
}

async fn wait_for(state: &State, id: &str, want: ServiceState, budget_ms: u64) -> bool {
    for _ in 0..(budget_ms / 20) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if svc_state(state, id) == Some(want) {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn payload_waits_for_cri_then_starts() {
    let state = State::new();
    let cri = Arc::new(FlipCri {
        ready: std::sync::atomic::AtomicBool::new(false),
    });

    // Publish RuntimeStatus the way RuntimeHealthController would (driving the
    // controller's 10s timer in-test is slow; the rule consumes the resource,
    // so publishing it directly keeps the test fast and equivalent).
    let publish_runtime_status = |state: &State, ready: bool| {
        let obj = machined_resources::ResourceObject::new(
            "runtime",
            "containerd",
            Resource::RuntimeStatus(RuntimeStatus {
                ready,
                name: "containerd".into(),
                version: "2.0.0".into(),
            }),
        );
        let k = Key::new("runtime", ResourceType::RuntimeStatus, "containerd");
        match state.get(&k) {
            Ok(cur) => {
                let _ = state.update(&k, cur.metadata.version, obj.spec);
            }
            Err(_) => {
                let _ = state.create(obj);
            }
        }
    };
    publish_runtime_status(&state, false);

    let services = vec![
        // Hermetic stand-in for containerd: long-running, harmless.
        ServiceConfig {
            id: "containerd".into(),
            command: vec!["sleep".into(), "30".into()],
            depends_on: vec![],
            restart: RestartPolicy::Never,
        },
        ServiceConfig {
            id: "payload".into(),
            command: vec!["true".into()],
            depends_on: vec!["containerd".into()],
            restart: RestartPolicy::Never,
        },
    ];

    let mut mgr = ServiceManager::new(state.clone());
    mgr.start_all(&services, Arc::new(RuntimeReadiness)).unwrap();

    // containerd (the stand-in) runs…
    assert!(
        wait_for(&state, "containerd", ServiceState::Running, 3000).await,
        "stand-in containerd should run"
    );
    // …but the payload stays Waiting: CRI not ready.
    assert!(
        wait_for(&state, "payload", ServiceState::Waiting, 3000).await,
        "payload should be Waiting while CRI is not ready"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        svc_state(&state, "payload"),
        Some(ServiceState::Waiting),
        "payload must NOT start before CRI is ready"
    );

    // Flip CRI ready (as the health controller would observe + publish).
    cri.ready.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(cri.ready().await.unwrap());
    publish_runtime_status(&state, true);

    // The payload now starts and finishes (command `true` → Finished).
    let started = wait_for(&state, "payload", ServiceState::Running, 5000).await
        || wait_for(&state, "payload", ServiceState::Finished, 5000).await;
    assert!(started, "payload must start once CRI is ready");

    mgr.stop_all().await;
}
```

- [ ] **Step 2: The docs example + README pointer**

Create `docs/examples/node-with-kubelet.yaml`:

```yaml
# Example machined-rs node config with a Kubernetes payload.
# machined is payload-agnostic: the kubelet below is any binary you ship in the
# image (a rusternetes kubelet is shown). `depends_on: [containerd]` is
# health-gated — the payload starts only when the machined-managed containerd
# is running AND its CRI reports RuntimeReady.
machine:
  hostname: node-1
  install:
    disk: /dev/sda
    wipe: false
  services:
    - id: kubelet
      command: [/usr/bin/rusternetes-kubelet, --node-name, node-1]
      depends_on: [containerd]
      restart: always
```

In `README.md`, add one paragraph (find the natural section — features/usage) pointing at the example:

```markdown
### Running a payload

machined supervises an external containerd and health-checks it over CRI. Any
config-declared service with `depends_on: [containerd]` starts only once the
runtime is genuinely ready (process up **and** CRI `RuntimeReady`). See
`docs/examples/node-with-kubelet.yaml` for a Kubernetes-payload example.
```

- [ ] **Step 3: Full gate + commit**

Run: `cargo test -p machined --test payload` → PASS.
Run: `make pre-commit` → fmt + clippy -D warnings + FULL workspace green (no hangs; ignored stay ignored).

```bash
git add crates/machined docs/examples README.md
git commit -m "test(machined): M4 acceptance e2e (payload gated on CRI ready) + docs example"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** `ServiceState::Waiting` + `ReadinessCheck`/`DefaultReadiness` (Running+healthy OR Finished) + `wait_for_deps` (200ms poll, indefinite, Waiting published, ~30s warn) + gated `start_all` (T1) ✓; `SequencerCtx.readiness` + `RuntimeReadiness` (containerd also needs `RuntimeStatus.ready`) + unit tests (T2) ✓; hermetic flip e2e (Waiting while not-ready → Running/Finished after flip) + docs example + README (T3) ✓.
- **Run-once semantics:** `Finished` counts as ready (spec-synced) — a completed init dep satisfies its dependents; a `Failed` dep blocks them (visible as `Waiting`).
- **Blast radius:** `start_all` callers (sequencer boot.rs + supervisor tests) and `SequencerCtx {` literals (boot.rs, shutdown.rs, boot_harness.rs, main.rs) — E0063/E0061 compile-driven; keep each commit workspace-green (T1 may absorb T2's mechanical fixes).
- **Hang risk in existing tests:** health gating changes dep semantics — T1 Step 4 explicitly audits dep-chain tests (a failing dep now hangs its dependent; fix the fixture, don't weaken the gate).
- **e2e honesty:** the e2e restates `RuntimeReadiness` (binary-private type) and publishes `RuntimeStatus` directly instead of driving the 10s controller timer — equivalent consumption path, fast and deterministic; the controller itself is already covered by M4a tests. The no-start-before-flip assertion (500ms hold in `Waiting`) is the key negative check.
- **Type consistency:** `ReadinessCheck`/`DefaultReadiness` (supervisor) ↔ `SequencerCtx.readiness` ↔ `RuntimeReadiness` (machined) ↔ `RUNTIME_SERVICE_ID`/`RuntimeStatus`. No new deps.
- **Placeholder scan:** none; complete code + exact commands throughout.
