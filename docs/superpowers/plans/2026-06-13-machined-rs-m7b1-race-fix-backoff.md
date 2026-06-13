# M7b-1 — PKI/STATE Race Fix + Restart Backoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Retire the PKI/STATE-mount race (PKI seeds + loads on the *mounted* ext4 STATE, not the shadowed initramfs) and give the supervisor per-service exponential restart backoff so an always-restart service can't hot-loop.

**Architecture:** Two independent changes in machined + supervisor, no image work. (1) A new `wait_for_state_mount` gate blocks PKI/API startup until the controller runtime publishes `MountStatus(STATE, mounted=true)`; `seed_pki` moves after that gate and gains fsync-for-durability. (2) `run_supervised`'s fixed 100 ms restart delay becomes a stop-aware exponential backoff (base 1 s → cap 30 s, reset after a service stays healthy ≥60 s), driven by a pure `backoff_step` helper.

**Tech Stack:** Rust, tokio (with `start_paused` test clock), the COSI `State` store, existing `Runner`/`ReadinessCheck` fakes.

**Plan-time verified facts (do not re-derive):**
- `run_supervised` (`crates/supervisor/src/service.rs:77-117`) is spawned **once per service** by the manager — so backoff is a **local loop variable**, not a shared map. Current delay: `let backoff = Duration::from_millis(100)` (line 86), slept at line 115 via a **non-stop-aware** `tokio::time::sleep(backoff)`.
- `should_restart(policy, outcome)` lives in `crates/supervisor/src/restart.rs:14-20`.
- `MountStatus` (`crates/resources/src/block.rs:46-54`): `{ volume, source, target, fstype, mounted: bool }`. The mount controller (`crates/controllers/src/block/mount.rs:82-92`) publishes it in namespace `"block"` with `volume: v.label` (so STATE's row has `volume == "STATE"`, `target == "/system/state"`, `mounted: true`).
- `State::list(ns, typ) -> Vec<ResourceObject>` (`crates/runtime-core/src/state.rs:60-68`); match `Resource::MountStatus(m)`.
- `run_daemon` (`crates/machined/src/main.rs`): pid1 block with `seed_pki` at ~lines 239-269 (KNOWN RACE comment ~256-262); runtime spawned via `tokio::spawn` at ~327-332 (`rt_handle`); PKI `load_or_generate` + API spawn at ~335-373; `provider` constructed at ~288 and not moved until the `SequencerCtx` at ~377; `state = runtime.state()` at ~293, not moved until ~377. So the window **after 332, before 335** still owns both `state` and `provider`.
- `provider.install() -> Option<&InstallSection>` (`crates/config/src/provider.rs:35-37`).
- `seed_pki` (`crates/machined/src/imageboot.rs:91-130`): all-or-nothing, never overwrites existing dst, atomic temp-dir + rename, 0700 dir / 0600 keys / 0644 certs. Consts `BOOT_PKI="/boot/pki"`, `BOOT_CONFIG`, `MODULES_LOAD` at lines 12-14.
- `wait_for_deps` poll cadence is 200 ms (`crates/supervisor/src/readiness.rs:56`).

---

### Task 1: Pure backoff step helper

**Files:**
- Modify: `crates/supervisor/src/restart.rs` (add `backoff_step` + tests next to `should_restart`)

- [ ] **Step 1: Write the failing tests** (append to the existing `#[cfg(test)] mod tests` in `restart.rs`, or add one)

```rust
#[cfg(test)]
mod backoff_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn first_failure_uses_base_then_doubles() {
        let (delay, next) = backoff_step(Duration::from_secs(1), Duration::from_secs(0));
        assert_eq!(delay, Duration::from_secs(1));
        assert_eq!(next, Duration::from_secs(2));
    }

    #[test]
    fn doubles_up_to_cap() {
        assert_eq!(backoff_step(Duration::from_secs(2), Duration::ZERO).1, Duration::from_secs(4));
        // 16s doubles to the 30s cap, not 32s.
        assert_eq!(backoff_step(Duration::from_secs(16), Duration::ZERO).1, Duration::from_secs(30));
        // cap holds.
        assert_eq!(backoff_step(Duration::from_secs(30), Duration::ZERO), (Duration::from_secs(30), Duration::from_secs(30)));
    }

    #[test]
    fn healthy_run_resets_to_base() {
        // Ran 61s (>= 60s threshold) before exiting → next restart waits base, not the escalated value.
        let (delay, next) = backoff_step(Duration::from_secs(30), Duration::from_secs(61));
        assert_eq!(delay, Duration::from_secs(1));
        assert_eq!(next, Duration::from_secs(2));
    }

    #[test]
    fn brief_run_keeps_escalating() {
        // Ran <60s → no reset; the current escalated delay is used.
        let (delay, _next) = backoff_step(Duration::from_secs(8), Duration::from_secs(5));
        assert_eq!(delay, Duration::from_secs(8));
    }
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined-supervisor backoff`
Expected: FAIL — `cannot find function backoff_step`.

- [ ] **Step 3: Implement `backoff_step`** (in `restart.rs`, above the tests)

```rust
use std::time::Duration;

/// Exponential restart backoff. Given the current backoff and how long the
/// service ran before exiting, return `(delay_to_sleep_now, next_backoff)`.
/// A service that stayed up at least `HEALTHY_RESET` is treated as recovered,
/// so its next restart waits the base delay rather than the escalated one —
/// long-lived services recover fast, while a binary that exits instantly keeps
/// escalating toward the cap instead of hot-looping.
pub fn backoff_step(current: Duration, ran_for: Duration) -> (Duration, Duration) {
    const BASE: Duration = Duration::from_secs(1);
    const CAP: Duration = Duration::from_secs(30);
    const HEALTHY_RESET: Duration = Duration::from_secs(60);
    let delay = if ran_for >= HEALTHY_RESET { BASE } else { current };
    let next = (delay * 2).min(CAP);
    (delay, next)
}
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p machined-supervisor backoff`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/supervisor/src/restart.rs
git commit -m "feat(supervisor): backoff_step — exponential restart backoff with healthy-reset"
```

---

### Task 2: Stop-aware backoff in run_supervised

**Files:**
- Modify: `crates/supervisor/src/service.rs` (the `run_supervised` loop + a `backoff_sleep` helper; mark restart-driving tests `start_paused`)

- [ ] **Step 1: Write the failing test** (append to `service.rs` tests) — proves a stop during backoff returns promptly rather than blocking for the full delay

First add this fake runner to the `service.rs` test module (alongside `Scripted`):

```rust
// Always reports Failure, so a restart loop enters backoff every iteration.
struct AlwaysFail(String);
#[async_trait]
impl Runner for AlwaysFail {
    fn id(&self) -> &str { &self.0 }
    async fn run(&mut self) -> crate::runner::Result<RunOutcome> { Ok(RunOutcome::Failure) }
    async fn stop(&mut self) -> crate::runner::Result<()> { Ok(()) }
}
```

Then the test:

```rust
#[tokio::test(start_paused = true)]
async fn stop_during_backoff_returns_promptly() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let state = State::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (stop2, state2) = (stop.clone(), state.clone());
    let h = tokio::spawn(async move {
        run_supervised(
            &state2,
            AlwaysFail("loop".into()),
            Policy::Always,
            stop2,
            Arc::new(DefaultReadiness),
            &[],
        )
        .await;
    });
    // Let it fail once and enter the backoff sleep, then request stop.
    tokio::time::sleep(Duration::from_millis(10)).await;
    stop.store(true, Ordering::SeqCst);
    // The timeout future measures REAL wall time even under start_paused, so a
    // genuine hang (a non-stop-aware sleep parked at the backoff delay) trips it;
    // a correct stop-aware backoff returns at once.
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .expect("must stop promptly")
        .unwrap();
    let k = Key::new("runtime", ResourceType::ServiceStatus, "loop");
    match state.get(&k).unwrap().spec {
        Resource::ServiceStatus(s) => assert_eq!(s.state, ServiceState::Stopped),
        _ => panic!(),
    }
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined-supervisor stop_during_backoff`
Expected: FAIL — today's `tokio::time::sleep(backoff)` is not stop-aware AND outcome restart uses the old fixed 100 ms; the test references behavior not yet present. (It may currently hang/penalize — that's the point.)

- [ ] **Step 3: Add `backoff_sleep` + rewire the loop** (`service.rs`)

Add the helper near the top of `service.rs`:

```rust
/// Sleep `dur`, but wake early if the stop intent is set — so a stop during a
/// long restart backoff is honoured promptly instead of holding stop_all for
/// the full delay. Mirrors the dep-gate select! already used below.
async fn backoff_sleep(stop: &std::sync::Arc<std::sync::atomic::AtomicBool>, dur: std::time::Duration) {
    use std::sync::atomic::Ordering;
    tokio::select! {
        () = tokio::time::sleep(dur) => {}
        () = async {
            while !stop.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        } => {}
    }
}
```

In `run_supervised`, replace `let backoff = Duration::from_millis(100);` (line ~86) with `let mut backoff = Duration::from_secs(1);` and replace the tail of the loop:

```rust
        let started = std::time::Instant::now();
        let outcome = run_service(state, &mut runner).await;
        if stop.load(Ordering::SeqCst) {
            publish_status(state, &id, ServiceState::Stopped, true, "drained");
            return;
        }
        if !should_restart(policy, outcome) {
            return;
        }
        let (delay, next) = crate::restart::backoff_step(backoff, started.elapsed());
        backoff = next;
        info!(service = %id, ?outcome, ?delay, "restarting service after backoff");
        backoff_sleep(&stop, delay).await;
```

(Keep the existing stop checks and `publish_status` calls above unchanged. `Instant` is `std::time::Instant`; under `start_paused` tokio's clock advances virtual time, and `Instant::now()` in tests still reflects it via tokio's time facade only if you use `tokio::time::Instant` — but `ran_for` here is just a coarse healthy/not-healthy threshold, and real tests drive failures back-to-back so `ran_for` is ~0; `std::time::Instant` is fine.)

- [ ] **Step 4: Mark restart-driving tests `start_paused` and run the suite**

Audit `crates/supervisor/src/*.rs` test modules for `run_supervised(` calls that now hit a restart (Policy::OnFailure/Always with ≥1 Failure before success/stop). For each, change `#[tokio::test]` → `#[tokio::test(start_paused = true)]` so the 1 s/2 s backoff sleeps fast-forward in virtual time. Known case: `supervised_on_failure_restarts_until_success` (`service.rs:197-224`). Leave run-once / no-restart tests as plain `#[tokio::test]`.

Run: `cargo test -p machined-supervisor`
Expected: PASS, and the suite finishes fast (no multi-second real sleeps).

- [ ] **Step 5: Commit**

```bash
git add crates/supervisor/src/service.rs
git commit -m "feat(supervisor): stop-aware exponential restart backoff (no more hot-loop)"
```

---

### Task 3: wait_for_state_mount gate

**Files:**
- Modify: `crates/machined/src/imageboot.rs` (add `wait_for_state_mount` + `state_mounted` + tests)

- [ ] **Step 1: Write the failing tests** (append to `imageboot.rs` tests)

```rust
#[tokio::test(start_paused = true)]
async fn wait_returns_true_when_state_mounted() {
    use machined_resources::{Resource, ResourceObject, MountStatus};
    use machined_runtime_core::State;
    let state = State::new();
    state.create(ResourceObject::new(
        "block",
        "STATE",
        Resource::MountStatus(MountStatus {
            volume: "STATE".into(),
            source: "/dev/vda2".into(),
            target: "/system/state".into(),
            fstype: "ext4".into(),
            mounted: true,
        }),
    )).unwrap();
    assert!(wait_for_state_mount(&state, std::time::Duration::from_secs(60)).await);
}

#[tokio::test(start_paused = true)]
async fn wait_times_out_when_state_absent_or_other_volume() {
    use machined_resources::{Resource, ResourceObject, MountStatus};
    use machined_runtime_core::State;
    let state = State::new();
    // An EPHEMERAL mount must NOT satisfy the STATE wait.
    state.create(ResourceObject::new(
        "block",
        "EPHEMERAL",
        Resource::MountStatus(MountStatus {
            volume: "EPHEMERAL".into(),
            source: "/dev/vda3".into(),
            target: "/var".into(),
            fstype: "ext4".into(),
            mounted: true,
        }),
    )).unwrap();
    // start_paused auto-advances the 200ms polls, so a 60s timeout resolves fast.
    assert!(!wait_for_state_mount(&state, std::time::Duration::from_secs(60)).await);
}

#[tokio::test(start_paused = true)]
async fn wait_unblocks_when_state_appears_mid_wait() {
    use machined_resources::{Resource, ResourceObject, MountStatus};
    use machined_runtime_core::State;
    let state = State::new();
    let s2 = state.clone();
    let waiter = tokio::spawn(async move {
        wait_for_state_mount(&s2, std::time::Duration::from_secs(60)).await
    });
    // Publish STATE after a virtual delay; the poll loop should then return true.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    state.create(ResourceObject::new(
        "block", "STATE",
        Resource::MountStatus(MountStatus {
            volume: "STATE".into(), source: "/dev/vda2".into(),
            target: "/system/state".into(), fstype: "ext4".into(), mounted: true,
        }),
    )).unwrap();
    assert!(waiter.await.unwrap());
}
```

Confirm `MountStatus` is re-exported from `machined_resources` (it's in `crates/resources/src/block.rs`); if the path differs, fix the `use` to the real one.

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined wait_`
Expected: FAIL — `cannot find function wait_for_state_mount`.

- [ ] **Step 3: Implement** (in `imageboot.rs`)

```rust
use machined_runtime_core::State;
use machined_resources::{Resource, ResourceType};

/// True iff a `MountStatus` for the STATE volume reports it mounted.
fn state_mounted(state: &State) -> bool {
    state
        .list("block", ResourceType::MountStatus)
        .into_iter()
        .any(|o| matches!(o.spec, Resource::MountStatus(m) if m.volume == "STATE" && m.mounted))
}

/// Block until the STATE volume is mounted (the controller runtime provisions
/// then mounts it), polling at the same 200ms cadence as the supervisor's
/// dep-gate. Returns false on timeout. Callers gate PKI seed/load on this so
/// PKI lands on the persistent ext4 STATE, not the initramfs rootfs it would
/// otherwise be shadowed on.
pub async fn wait_for_state_mount(state: &State, timeout: std::time::Duration) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        if state_mounted(state) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p machined wait_`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/machined/src/imageboot.rs
git commit -m "feat(machined): wait_for_state_mount — gate on STATE MountStatus"
```

---

### Task 4: fsync durability in seed_pki

**Files:**
- Modify: `crates/machined/src/imageboot.rs` (`seed_pki`: fsync staged files + parent dir)

- [ ] **Step 1: Adjust the existing happy-path test to assert content survives** (the durability itself isn't unit-observable, but pin that the seed still produces complete, correct files after the fsync additions)

In `seeds_pki_from_boot_when_state_pki_missing` (imageboot.rs ~243-274), after the existing assertions add (if not already present):

```rust
    // Each seeded file is non-empty and matches the source (fsync path must not
    // truncate or drop content).
    for f in ["ca.pem", "ca.key", "server.pem", "server.key"] {
        let got = std::fs::read(dst.join(f)).unwrap();
        assert!(!got.is_empty(), "{f} empty after seed");
        assert_eq!(got, std::fs::read(src.join(f)).unwrap(), "{f} content mismatch");
    }
```

(Adapt `src`/`dst` to the test's actual variable names.)

- [ ] **Step 2: Run, verify current test still passes (baseline)**

Run: `cargo test -p machined seeds_pki_from_boot`
Expected: PASS (baseline before adding fsync).

- [ ] **Step 3: Add fsync to `seed_pki`** — between the copy loop and the rename, and after the rename for the parent dir

```rust
    // fsync each staged file before the rename: ext4's auto_da_alloc heuristic
    // does NOT cover rename-to-a-new-path, so without this a power cut just
    // after the rename could leave a present dst dir shadowing zero-length keys
    // — which load_or_generate would then read as a partial PKI (PkiError) and
    // disable the API forever.
    for f in FILES {
        std::fs::File::open(tmp.join(f))
            .and_then(|fh| fh.sync_all())
            .with_context(|| format!("fsync {f}"))?;
    }
    std::fs::rename(&tmp, dst)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dst.display()))?;
    // Persist the rename itself by syncing the parent directory.
    if let Some(parent) = dst.parent() {
        let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
    }
    info!("seeded PKI from {}", src.display());
    Ok(())
```

(Replace the existing `std::fs::rename(...)?;` + `info!` tail with the block above; `FILES` and `tmp`/`dst`/`Context` are already in scope.)

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p machined imageboot`
Expected: PASS — all imageboot tests including the strengthened seed test.

- [ ] **Step 5: Commit**

```bash
git add crates/machined/src/imageboot.rs
git commit -m "fix(machined): fsync seeded PKI before+after rename (durability)"
```

---

### Task 5: Rewire run_daemon — seed after STATE mount

**Files:**
- Modify: `crates/machined/src/main.rs` (`run_daemon`: remove `seed_pki` from the pid1 block; insert the wait+seed block after the runtime spawn, before the PKI block; rewrite the KNOWN RACE comment)

This task is the integration wiring; it has **no new unit test** (main.rs `run_daemon` has no test harness — its correctness here is a mechanical reorder covered by Task 3's `wait_for_state_mount` tests and validated end-to-end by the M7b-2 QEMU boot test). The existing main.rs tests (reset pins, park behavior) must still pass.

- [ ] **Step 1: Remove `seed_pki` from the pid1 block**

In the pid1 block (~lines 239-269), delete the `seed_pki` call and its surrounding KNOWN RACE comment:

```rust
        // DELETE these lines:
        // KNOWN RACE (M7b): ... (the whole comment block ~256-262)
        if let Err(e) = imageboot::seed_pki(Path::new(imageboot::BOOT_PKI), Path::new("/system/state/pki")) {
            error!("pki seed: {e}");
        }
```

Leave `mount_essential`, `load_modules`, `mount_boot` in place.

- [ ] **Step 2: Insert the wait+seed block after the runtime spawn**

Immediately before the PKI block (the `let pki_dir = ...` / `match NodePki::load_or_generate` at ~335-340), add:

```rust
    // M7b: PKI must be seeded and loaded on the MOUNTED STATE volume, not the
    // initramfs rootfs (where it would be shadowed by the later STATE mount,
    // letting load_or_generate mint a fresh CA and lock out the baked client
    // bundle). The controller runtime spawned above provisions + mounts STATE;
    // wait for that, then seed. pid1-gated and only when an install disk is
    // configured, so dev/test runs are unaffected.
    if std::process::id() == 1 && provider.install().is_some() {
        if !imageboot::wait_for_state_mount(&state, std::time::Duration::from_secs(60)).await {
            warn!("STATE volume not mounted within 60s; PKI may not persist across reboots");
        }
        if let Err(e) =
            imageboot::seed_pki(Path::new(imageboot::BOOT_PKI), Path::new("/system/state/pki"))
        {
            error!("pki seed: {e}");
        }
    }
```

Verify `warn!` is imported in main.rs (it uses `error!`/`info!`; add `warn` to the `use tracing::{...}` if absent). `provider` and `state` are both still owned at this point (moved into `SequencerCtx` later).

- [ ] **Step 3: Build + clippy + existing tests**

Run: `cargo build -p machined && cargo clippy -p machined --all-targets -- -D warnings && cargo test -p machined`
Expected: clean build, no clippy warnings, all existing main.rs + imageboot tests pass.

- [ ] **Step 4: Workspace gate**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add crates/machined/src/main.rs
git commit -m "fix(machined): seed+load PKI after STATE mount (closes the PKI/STATE race)"
```

---

### Task 6: Finish

- [ ] **Step 1: Full gates**

Run: `make pre-commit` (fmt + clippy -D warnings + workspace test)
Expected: clean.

- [ ] **Step 2: Finishing**

Follow superpowers:finishing-a-development-branch. Merge via PR (CI green: the `check` job covers these changes; the in-container boot-test still runs the current cold-boot flow and must stay green — the race fix doesn't change cold-boot behavior, only removes the warm-boot re-key hazard). Merge to main, delete branch.

---

## Verification (end-to-end)

1. `cargo test --workspace` green, incl: `backoff_step` (4), stop-during-backoff (1), `wait_for_state_mount` (3), strengthened `seed_pki` test, existing supervisor restart tests under `start_paused`.
2. `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo fmt --all --check` clean.
3. CI: `check` + in-container `boot-test` both green (cold boot still passes — the seed now happens post-mount, which on a cold boot is moments later in the same boot; the boot test asserts the API comes up, which it must).
4. Behavioural intent (not separately CI-tested here; proven by M7b-2's runtime bring-up and available for a future warm-boot CI pass): a second boot reuses the STATE-persisted CA instead of minting a fresh one.

## Known gaps / notes

- The race fix's true end-to-end proof is a **warm reboot** showing the same CA — deferred (optional follow-up). M7b-1 ships the mechanism + unit coverage; the cold-boot CI continues to pass.
- Backoff base 1 s means the first restart is slower than the old 100 ms — intentional (anti-hot-loop); containerd and payloads tolerate it.
- `wait_for_state_mount` uses a 60 s timeout then proceeds best-effort — a node that can't mount STATE still brings the API up (on non-persistent storage) rather than hanging PID 1.
