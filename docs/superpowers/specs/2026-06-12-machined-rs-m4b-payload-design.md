# machined-rs M4b — Payload Bring-up (health-gated depends_on), Design

**Date:** 2026-06-12
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (completes milestone M4: container runtime + payload)
**Builds on:** M4a (containerd supervised + CRI `RuntimeStatus`), merged to `main`.

## 1. Overview

M4b makes a config-declared payload (e.g. a rusternetes kubelet) start **only once its dependencies
are genuinely ready** — not merely process-alive. The supervisor's `depends_on` becomes
health-gated via an injected `ReadinessCheck`; the rule for the built-in `containerd` service
additionally requires `RuntimeStatus.ready` (the M4a CRI probe). A waiting service is observable as
`ServiceState::Waiting`. machined stays payload-agnostic: the payload is just config; a documented
example shows a rusternetes kubelet declaration. This completes milestone M4.

## 2. Goals / Non-goals

### Goals
- `ServiceState::Waiting` resource variant.
- A `ReadinessCheck` trait in `supervisor` + `default_readiness` (dep's `ServiceStatus` is `Running`
  **and** `healthy`); `start_all` takes the check; each service task waits (polling, indefinite,
  `Waiting` published) for its deps before running.
- The machined-level rule: containerd's readiness additionally requires `RuntimeStatus.ready`.
- An e2e proving a payload starts only after the runtime readiness flips true.
- A documented example node config declaring a kubelet payload (`docs/examples/`).

### Non-goals (deferred)
- Per-service readiness probes for arbitrary services (HTTP/exec health checks) — a later milestone;
  M4b's per-service health stays process-alive except the containerd rule.
- Dep-wait timeouts (indefinite wait is deliberate; the `Waiting` state makes a stuck dep visible).
- Restarting dependents when a dependency dies (no dependency-aware restart cascade).
- Shipping/booting actual rusternetes binaries in-tree or CI (agnostic; docs example only).
- Graceful SIGTERM stop ordering (M5, as before).

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `resources` | + `ServiceState::Waiting` (apiserver renders states via `{:?}` — no mapping change). |
| `supervisor` | `readiness.rs`: `ReadinessCheck` trait + `DefaultReadiness`; `start_all(services, check)`; the per-service task waits for deps (200ms poll, periodic log) publishing `Waiting`, then runs. |
| `machined` | `RuntimeReadiness` check (default rule; for `RUNTIME_SERVICE_ID` also `RuntimeStatus.ready`); constructed in `run_daemon` and threaded via `SequencerCtx`. |
| `sequencer` | `SequencerCtx` carries the `Arc<dyn ReadinessCheck>`; StartServices passes it to `start_all`. |
| docs | `docs/examples/node-with-kubelet.yaml` + a short README note. |

### 3.2 `ReadinessCheck`

```rust
/// Decides when a dependency service is ready to be depended on.
pub trait ReadinessCheck: Send + Sync {
    /// True iff `dep_id` is ready (its dependents may start).
    fn is_ready(&self, state: &State, dep_id: &str) -> bool;
}

/// Default: the dep's ServiceStatus exists, state == Running, healthy == true.
pub struct DefaultReadiness;
```

`start_all(&mut self, services, check: Arc<dyn ReadinessCheck>)`. Each spawned service task, before
running its `Runner`:

```text
if !deps.is_empty():
    publish_status(id, Waiting, healthy=false, "waiting for deps: …")
    loop every 200ms until deps.iter().all(|d| check.is_ready(&state, d))
        (warn-log every 150 ticks ≈ 30s with the not-ready deps)
then run the runner as today (Starting → Running …)
```

`start_all` still spawns everything immediately and returns — the boot sequencer never blocks; the
gating happens inside each service's own task. `stop_all` is unchanged (aborting a Waiting task is
trivially safe — no child yet).

### 3.3 The machined rule (`RuntimeReadiness`)

```rust
/// Default readiness, plus: the built-in containerd service is only ready once
/// the CRI probe reports the runtime ready (RuntimeStatus.ready).
struct RuntimeReadiness;
impl ReadinessCheck for RuntimeReadiness {
    fn is_ready(&self, state, dep_id) -> bool {
        let base = DefaultReadiness.is_ready(state, dep_id);
        if dep_id == RUNTIME_SERVICE_ID {
            base && runtime_status_ready(state)   // RuntimeStatus "containerd".ready
        } else {
            base
        }
    }
}
```

Lives in `machined` (next to the containerd injection); the supervisor stays runtime-agnostic.
`SequencerCtx` gains `readiness: Arc<dyn ReadinessCheck>`; machined constructs `RuntimeReadiness`,
tests construct `DefaultReadiness` (or a stub).

### 3.4 Payload flow (the M4 finish line)

```
boot → StartServices spawns [containerd, payload(depends_on: containerd), …]
  payload task: publishes Waiting (deps not ready)
  containerd task: runs the binary → ServiceStatus Running+healthy
  RuntimeHealthController: CRI probe → RuntimeStatus.ready = true
  payload task: RuntimeReadiness(containerd) now true → runs → Running
```

Observable end-to-end: `machinectl get ServiceStatus` shows `Waiting → Running`; `get RuntimeStatus`
shows the gate.

### 3.5 Docs example (`docs/examples/node-with-kubelet.yaml`)

```yaml
machine:
  hostname: node-1
  install: { disk: /dev/sda, wipe: false }
  services:
    - id: kubelet                       # any payload binary; rusternetes shown
      command: [/usr/bin/rusternetes-kubelet, --node-name, node-1]
      depends_on: [containerd]          # starts only when CRI reports ready
      restart: always
```

With a paragraph explaining the gate (containerd readiness = process up **and** CRI RuntimeReady).

## 4. Error handling & observability

- A service whose dep never becomes ready stays `Waiting` forever — visible via the API and logged
  every ~30s with the offending dep ids. Deliberate (no arbitrary timeout).
- `Waiting` publishes `healthy: false`, so a dependent-of-a-dependent also waits (transitive gating
  falls out of `DefaultReadiness` for free).
- A dep that dies after its dependent started: no cascade (non-goal); the dep's own restart policy
  applies.

## 5. Testing strategy

- **Unit (supervisor, root-free):** `DefaultReadiness` truth table (absent / Waiting / Running+unhealthy
  / Running+healthy); a two-service `start_all` where the dependent stays `Waiting` while the dep is
  not ready and starts once a test readiness-stub flips; `stop_all` while `Waiting` is clean.
- **Unit (machined):** `RuntimeReadiness` — containerd requires both `ServiceStatus` Running+healthy
  AND `RuntimeStatus.ready`; non-containerd ids use the default rule only.
- **e2e (root-free, hermetic):** real `Runtime` + `RuntimeHealthController` (a `FakeCriClient` that
  starts not-ready) + a real `ServiceManager` with `RuntimeReadiness`, services = a stand-in
  `containerd` (`ServiceConfig { id: "containerd", command: ["sleep","30"] }`, constructed directly —
  hermetic, no real containerd) and a `payload` (`["true"]`, `depends_on: [containerd]`). Assert the
  payload's `ServiceStatus` is `Waiting` while CRI is not-ready, then flip the fake → payload reaches
  `Running`. This is the M4 acceptance test.
- **CI:** `make pre-commit`.

## 6. Key risks

- **`start_all` signature change** — every `start_all` call site (sequencer + supervisor tests +
  boot harness) gains the check arg; `SequencerCtx` gains a field (all constructors break — grep
  `SequencerCtx {`). Compile-driven follow-through.
- **`ServiceState::Waiting` exhaustive matches** — adding the variant breaks any exhaustive match on
  `ServiceState` (grep; the apiserver uses `{:?}` so it's unaffected).
- **Poll-loop liveness** — the waiting task must yield (tokio sleep) and react to `stop_all` abort
  (it does — abort cancels the sleep). The e2e pins the flip actually unblocking the loop.
- **Fake-flip e2e timing** — flip the fake only after asserting `Waiting` was observed; generous poll
  budgets (the established pattern).
