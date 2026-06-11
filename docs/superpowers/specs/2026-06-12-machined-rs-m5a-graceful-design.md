# machined-rs M5a — Graceful Shutdown, Design

**Date:** 2026-06-12
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (begins milestone M5: lifecycle)
**Builds on:** M0–M4 merged to `main`.

## 1. Overview

M5a makes stopping the node as disciplined as starting it: services receive `SIGTERM` and a
per-service grace period before being killed, stop in reverse start order (dependents drain before
their dependencies), the restart loop re-checks dependency readiness before every re-run (closing the
M4b note), volumes are synced and unmounted on shutdown (the deferred M2b-2b unmount), and the API
server task is shut down and joined (the carried-forward M3a-2 item). Reset is M5b; upgrade/kexec is
deferred until an image pipeline exists.

## 2. Goals / Non-goals

### Goals
- `ServiceConfig.stop_grace_secs` (default 10).
- Supervisor graceful stop: stop-intent → `SIGTERM` → grace → abort (`kill_on_drop` = `SIGKILL`),
  reverse start order, sequential.
- `RestartRunner` pre-run hook awaited before **every** attempt; the manager passes the dep-wait —
  one gate for first start and restarts.
- `Platform::unmount(target)` + `Platform::sync()` (real `nix` umount2/sync; fake records).
- Shutdown sequence: graceful stop → sync → unmount `/var`, `/system/state`, `/boot` (reverse mount
  order, best-effort with logged failures).
- machined: `apiserver` served with a shutdown token; the task joined during shutdown.

### Non-goals (deferred)
- **Reset** (M5b), upgrade/kexec (needs an image pipeline).
- Parallel-with-dependency-ordering stop (sequential reverse order is sufficient and simpler).
- Re-mounting/remount-ro fallbacks when unmount fails (log + continue; the disk is journaled).
- Stop-time `preStop` hooks, per-service stop signals other than TERM.
- Restarting dependents when a dependency stops (still a non-goal from M4b).

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `config` | + `stop_grace_secs: Option<u64>` on `ServiceConfig` (serde default `None` → 10s effective). |
| `supervisor` | `ProcessRunner` shares its child PID via an `Arc<Mutex<Option<u32>>>` slot; `RestartRunner` gains a stop-intent (`Arc<AtomicBool>`) + an optional async pre-run hook; `ServiceManager` records per-service handles `{ join, pid_slot, stop_intent, grace }` and `stop_all` walks them in reverse. |
| `platform` | + `unmount(&self, target) -> Result<()>` + `sync(&self)`; `LinuxPlatform` = `nix::mount::umount2(MNT_DETACH? no — plain umount, MNT_DETACH only as documented fallback…)` **plain `umount2(target, MntFlags::empty())`**, `nix::unistd::sync()`; `FakePlatform` records unmounts + sync calls (and removes from its mounts list so `is_mounted` flips). |
| `sequencer` | shutdown phases: `stop` (graceful stop_all) → `disk` (new `SyncAndUnmount` task: `platform.sync()`, then unmount `/var`, `/system/state`, `/boot` if mounted; best-effort, warn on failure). |
| `machined` | the apiserver spawn uses `serve_with_shutdown(addr, …, token)`; `run_daemon` cancels the token during shutdown and joins the API task (bounded, e.g. 5s). |

### 3.2 Supervisor: graceful stop mechanics

Per service the manager keeps:

```text
ServiceHandle { id, join: JoinHandle, pid: Arc<Mutex<Option<u32>>>, stop: Arc<AtomicBool>, grace: Duration }
```

- `ProcessRunner` writes the child PID into `pid` after spawn, clears it after `wait()` returns.
- `RestartRunner::run` checks `stop` before every (re-)run: if set, return `Stopped` instead of
  re-running. It also awaits the pre-run hook (the dep-gate) before every attempt.
- `stop_all` (reverse start order, sequential per service):
  1. `stop.store(true)` — no further restarts.
  2. If `pid` holds a live PID → `kill(pid, SIGTERM)` (via `nix`; ESRCH ignored).
  3. `tokio::time::timeout(grace, &mut join)`:
     - finished → service drained gracefully (`ServiceStatus` Finished via the normal path).
     - timeout → `join.abort()` — dropping the runner SIGKILLs the child (`kill_on_drop`), as today.
  4. Continue to the next (older) service.
- A service still `Waiting` (no child) just gets aborted as today — trivially safe.

### 3.3 Restart re-wait (the unified gate) — `run_supervised`

A pre-run hook inside `RestartRunner` would be status-dishonest: `run_service` publishes
`Preparing→Running` once up front, so a restart parked on the gate would still show `Running`.
Instead the restart loop moves up a layer into a new `run_supervised` (service.rs):

```text
run_supervised(state, inner: ProcessRunner-like, policy, stop: Arc<AtomicBool>,
               check: Arc<dyn ReadinessCheck>, deps: &[String])
loop:
    if stop → publish Finished "stopped"; return
    wait_for_deps(state, check, id, deps).await      // publishes Waiting if gated
    if stop → return                                  // stop won while gated
    outcome = run_service(state, &mut inner).await    // per-ATTEMPT Preparing→Running→…
    if !should_restart(policy, outcome) → return
    sleep(backoff)
```

Every attempt (first start and each restart) gets the same gate AND truthful per-attempt status
(`Waiting → Preparing → Running → …`, cycling on restarts). `RestartRunner` is retired — its policy
logic survives as a pure `should_restart(policy, outcome)` (its scripted tests adapt to
`run_supervised`); the manager spawns `run_supervised` directly. `wait_for_deps` keeps its fast
path, so ready deps add no restart latency.

### 3.4 Platform unmount + shutdown disk task

```text
Platform::unmount(&self, target: &str) -> Result<()>     // umount2(target, empty)
Platform::sync(&self)                                      // sync(2), infallible
```

`SyncAndUnmount` (sequencer, after `stop`): `platform.sync()`; then for `["/var", "/system/state",
"/boot"]` (reverse mount order): if `platform.is_mounted(t)` → `platform.unmount(t)`, warning on
error and continuing (best-effort; never blocks shutdown). The fake records the sequence so tests
can assert sync-before-unmount and the reverse order.

### 3.5 machined: API-task shutdown

`apiserver::serve` gains a sibling `serve_with_shutdown(addr, state, version, pki, actions, signal:
impl Future)` using tonic's `serve_with_incoming_shutdown`/`serve_with_shutdown`. machined passes
`shutdown.cancelled()` (the existing CancellationToken) and keeps the `JoinHandle`; during shutdown
(after the sequencer) it awaits the handle with a 5s bound (then aborts). The plain `serve` remains
for compatibility (tests use it).

## 4. Error handling & observability

- `SIGTERM` delivery failures other than ESRCH are logged; the grace/abort path still bounds stop.
- Unmount failures: warn + continue (best-effort; shutdown must complete).
- Stop progress is observable: each service's `ServiceStatus` transitions through Finished/Failed as
  it drains; the daemon logs per-service stop outcomes (drained vs killed).

## 5. Testing strategy

- **Unit (supervisor, root-free, real processes):**
  - Graceful drain: a service running `sh -c 'trap "exit 0" TERM; sleep 30'` with grace 5s — `stop_all`
    returns well under the grace, the child exited via TERM (no abort), status Finished.
  - Grace expiry → kill: `sh -c 'trap "" TERM; sleep 30'` with grace 1s — `stop_all` returns ~1s,
    task aborted (kill_on_drop reaps), no orphan process (assert the PID is gone).
  - Stop-intent: a `restart: always` short-lived service stops restarting once stop is set.
  - Reverse order: two services with a dependency; the fake/status order of stops is dependent-first
    (assert via stop timestamps or a recording wrapper).
  - Re-wait: a gated service whose dep flips not-ready after its first run — the restart parks
    (Waiting) instead of re-running; flips back → re-runs.
- **Unit (platform):** fake unmount flips `is_mounted` + records; `LinuxPlatform::unmount` of a
  non-mounted path errors (root-free negative); sync callable.
- **Sequencer:** shutdown e2e on the fake — order is stop → sync → unmount(/var) →
  unmount(/system/state) → unmount(/boot), only for mounted targets, failures logged not fatal.
- **machined:** the API task exits when the token cancels (root-free: serve on a loopback port,
  cancel, join completes within the bound).
- **CI:** `make pre-commit`.

## 6. Key risks

- **PID reuse / kill-after-exit** — the PID slot is cleared when `wait()` returns and ESRCH is
  ignored, but a recycled PID between exit and clear is theoretically possible; mitigation: the slot
  is cleared before `run()` returns and stop reads it once. Acceptable for M5a (same exposure as
  every PID-based init).
- **`RestartRunner` restructure** — moving the gate inside the runner touches the M1 core; the
  existing supervisor tests (16) must stay green; the stop-intent must not race the gate (check stop
  both before the gate and after it).
- **Unmount of busy filesystems** — `/var` may be busy if a service ignored SIGKILL semantics
  (zombie IO); best-effort + warn keeps shutdown bounded. MNT_DETACH (lazy) is documented as the
  M5b/M6 escalation if real-world busy-unmounts bite.
- **tonic shutdown API** — `serve_with_shutdown` exists on tonic 0.12 Router; minor signature
  variance is contained to `apiserver::serve_with_shutdown` (the one new function).
