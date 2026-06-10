# machined-rs — Design

**Date:** 2026-06-10
**Status:** Approved (brainstorming) — first spec covers Milestones M0 + M1
**Repo:** standalone cargo workspace at `/home/jones/PhpstormProjects/machined-rs`

## 1. Overview

`machined-rs` is a clean-break, greenfield Rust reimplementation of the *node-OS half* of
[Talos Linux's `machined`](https://github.com/siderolabs/talos/tree/main/internal/app/machined).
It is the PID 1 / init system and machine-management daemon for an immutable Rust node OS whose
purpose is to boot a node and run [`rusternetes`](../rusternetes) — a from-scratch Rust Kubernetes —
on top.

It is **inspired by** Talos machined, not wire-compatible with it. We are free to redesign the
machine config format, the management API, and the internal resource model. We are **not** porting
Talos's Kubernetes-facing machinery (k8s controllers, etcd membership/snapshot, kubespan, siderolink,
discovery, trustd/apid split). rusternetes replaces all of that and keeps cluster state in SQLite,
not etcd.

### Target environment

The north-star deployment (inherited from rusternetes) is a 4-node cluster of Raspberry Pi 3A+
boards: 512 MB RAM, quad-core Cortex-A53, USB-ethernet, a single 128 GB micro-SD per node holding
OS + binary + state. **Footprint discipline is a first-class design constraint**: small RSS, few
processes, minimal SD write amplification.

## 2. Goals / Non-goals

### Goals
- Run as PID 1: mount essential filesystems, configure the kernel, reap zombies, handle
  emergency/shutdown/reboot.
- Provide a **generic reconcile runtime** (COSI-like semantics, statically typed) that all
  node subsystems are built on.
- Drive a **lifecycle sequencer** (boot / reset / upgrade / reboot / shutdown phases).
- **Supervise services**: an external `containerd` (via CRI) and the `rusternetes` binary, plus
  internal tasks — with health checks, restart policy, ordered dependency-aware shutdown.
- Configure the node: network (netlink), block storage (partition/format/mount the SD), node PKI,
  hostname/DNS, time.
- Load a **clean-break machine config** and expose it to controllers via a `Provider` trait.
- Expose a **management gRPC API** (talosctl-equivalent) and a `machinectl` CLI.

### Non-goals (for this project / explicitly out of scope)
- Kubernetes control-plane logic, etcd, kubelet/CRI implementation — those live in rusternetes.
- Wire-compatibility with Talos's machine config, COSI resource format, or `machine.MachineService`.
- Talos's CEL config validation, version-contract machinery, multidoc config.
- kubespan / siderolink / discovery / cluster-membership features.
- Running pods directly — machined-rs supervises the container runtime, it is not the runtime.

## 3. Architecture

### 3.1 Crate layout (standalone workspace, mirrors rusternetes conventions)

Toolchain matches rusternetes: edition 2021, `tokio`, `tonic` 0.12 / `prost` 0.13, `rustls`
(aws-lc-rs), `tracing`, `cargo fmt`/`clippy -D warnings`, `make pre-commit`.

| crate | role | Talos analog |
|---|---|---|
| `machined` | PID 1 binary; multi-call entry; wires runtime + sequencer + supervisor + api | `internal/app/machined` |
| `runtime-core` | generic reconcile runtime: resource store, watch, controller loop, finalizers, owners | `cosi-project/runtime` |
| `resources` | typed resource definitions as a closed enum registry | `pkg/machinery/resources` |
| `config` | machine config parse / validate / persist + `Provider` trait | `pkg/machinery/config` |
| `supervisor` | service manager + runners (process / containerd / in-proc task), health, events | `pkg/system` |
| `sequencer` | lifecycle phases & tasks | `pkg/runtime/v1alpha1` |
| `controllers` | node controllers: net, block, secrets, files, time, hw (modules) | `pkg/controllers/*` |
| `platform` | hardware/platform abstraction, kernel cmdline, sysctl, mount, netlink wrappers | `pkg/runtime/.../platform` |
| `apiserver` | tonic gRPC management API (clean-break proto) | `internal/server/v1alpha1` |
| `machinectl` | CLI client | `talosctl` |
| `common` | errors, logging, shared helper types | `siderolabs/gen`, `zap` |

Dependency direction: `common` ← `runtime-core` ← `resources` ← {`controllers`, `sequencer`,
`supervisor`, `config`, `apiserver`} ← `machined`. `platform` is a leaf used by controllers and
sequencer.

### 3.2 runtime-core — the generic reconcile runtime

Keeps COSI **semantics**, but statically typed for a single embedded binary (no protobuf-`Any`,
no reflection).

- **Resource model:** every resource has metadata `{ namespace, type, id, version, owner,
  finalizers, phase }` plus a typed spec. The known resource set is a **closed enum**:
  ```rust
  enum Resource {
      MachineConfig(Versioned<MachineConfigSpec>),
      NodeAddress(Versioned<NodeAddressSpec>),
      RouteStatus(Versioned<RouteSpec>),
      ServiceStatus(Versioned<ServiceStatusSpec>),
      MountStatus(Versioned<MountSpec>),
      // ...
  }
  ```
  Adding a type edits the enum — the compiler then forces every exhaustive match to handle it.
  This is the deliberate trade vs COSI's open plug-in registry: safety + size over extensibility,
  correct for one binary.
- **State store:** in-memory map keyed by `(namespace, type, id)` → versioned resource. Operations:
  `get`, `list`, `watch`, `create`, `update` (CAS on version), `destroy`, plus `add_finalizer` /
  `remove_finalizer` and `teardown` (mark for deletion, hold while finalizers remain).
- **Watch:** per-key version counters + a `tokio::sync::broadcast` event bus emitting
  `{Created, Updated, Destroyed}` events filtered by type/namespace. Controllers subscribe to their
  declared inputs.
- **Controllers:**
  ```rust
  trait Controller {
      fn name(&self) -> &str;
      fn inputs(&self) -> Vec<Input>;   // strong (depends-on, gets destroy via finalizer) | weak (watch only)
      fn outputs(&self) -> Vec<Output>; // exclusive (one writer) | shared (per-owner)
      async fn run(&mut self, ctx: &mut ReconcileCtx) -> Result<()>;
  }
  ```
  A single-threaded-per-controller reconcile loop wakes the controller on any input event (debounced),
  runs `run` to convergence, and enforces output ownership. Owner refs + finalizers give ordered
  teardown (a strong input's destroy is deferred until the dependent removes its finalizer).
- **Runtime:** registers all controllers, builds the implicit input/output dependency graph, spawns
  each controller as a tokio task, and provides the shared `State` handle.

### 3.3 config — clean-break machine config

- Format: **single-document YAML**, deserialized with `serde` into typed structs. No multidoc, no
  version contracts, no CEL.
- Sources (in precedence): kernel cmdline override → STATE partition → platform/metadata → defaults.
- `trait Provider` gives controllers a read-only, snapshot view (`machine()`, `network()`,
  `cluster()`, …). The sequencer owns load + persist (to STATE) + atomic apply.
- Config is itself surfaced as a `Resource::MachineConfig` so controllers reconcile against it via
  the normal watch mechanism.

### 3.4 supervisor — service management

- `ServiceManager`: load / start / stop / restart / list services; builds reverse-dependency map for
  ordered, grace-bounded shutdown; surfaces each service as `Resource::ServiceStatus`.
- `ServiceRunner`: drives one service through `Pre → wait(Condition) → Run → health` with state
  transitions (Preparing / Running / Finished / Skipped / Failed) and event recording.
- `trait Runner { open / run / stop / close }` with backends:
  - `process` — fork/exec a host process (M1).
  - `containerd` — run/managed via CRI (M4).
  - `task` — in-process tokio task (e.g. the apiserver).
  - `restart` — wrapper adding restart-on-failure policy.
- Health checks and events feed `ServiceStatus` resources, so the API and sequencer observe service
  state through the runtime, not ad-hoc channels.

### 3.5 sequencer — lifecycle

- A phase/task model: ordered `PhaseList`s of idempotent tasks, with conditional
  (`append_when`) and deferred-check appends.
- Sequences: **Boot**, **Reset**, **Upgrade**, **Reboot**, **Shutdown**, **EmergencyCleanup**.
- The sequencer is the imperative glue that the declarative controllers sit inside: it brings the
  runtime up, mounts disks, then lets controllers reconcile steady-state, and tears down in order on
  exit.

## 4. Milestone roadmap

- **M0 — Foundation:** workspace scaffold, `common`, `runtime-core` (store + watch + controller loop
  + finalizers/owners) with unit tests, `resources` skeleton.
- **M1 — First boot slice:** PID 1 (mount essential fs, sysctl, signal/zombie reaping, emergency
  halt), minimal `config` load, `sequencer` Boot phase, `supervisor` running one `process` service.
  Goal: boots in a VM, launches a dummy service, reconcile loop live, clean shutdown.
- **M2 — Node controllers:** network (netlink: link/addr/route/hostname/resolv), block
  (discover/partition/format/mount BOOT/STATE/EPHEMERAL), time sync.
- **M3 — API + secrets:** node PKI (rcgen/rustls), tonic management API
  (version/services/logs/reboot/apply-config), `machinectl`.
- **M4 — Workload bring-up:** supervise `containerd` via CRI + supervise `rusternetes`;
  full boot → running rusternetes node.
- **M5 — Lifecycle:** reset, A/B image upgrade + kexec, reboot/shutdown, config rollback.
- **M6 — Hardening:** health/events/watchdog, emergency volume cleanup, multi-node, install image.

Each milestone after M1 gets its own spec → plan → implement cycle.

## 5. First spec scope — M0 + M1 (detailed)

This is what the first implementation plan will cover.

### M0 deliverables
1. Cargo workspace + the crate skeletons above; `make pre-commit` (fmt + clippy -D warnings + test)
   green on an empty build.
2. `common`: `Error`/`Result`, `tracing` setup, small shared types.
3. `runtime-core`:
   - `Resource` enum (seed types: `MachineConfig`, `ServiceStatus`) + `Metadata` + `Versioned<T>`.
   - `State` store with `get/list/watch/create/update(CAS)/destroy` + finalizers + owner refs.
   - `broadcast`-based watch with type/namespace filtering.
   - `Controller` trait + `Runtime` that registers controllers, spawns reconcile loops, and wires
     the dependency graph.
   - Unit tests: CAS conflict, watch delivery ordering, finalizer-gated teardown, a toy controller
     reconciling a derived resource.

### M1 deliverables
1. `machined` binary as PID 1 entrypoint (multi-call dispatch on `argv[0]`/subcommand).
2. `platform` minimal: mount essential filesystems (`/proc`, `/sys`, `/dev`, `/tmp`, `/run`), apply a
   baseline set of sysctls, read kernel cmdline.
3. PID 1 duties: reap zombies (`SIGCHLD`/`waitpid` loop), handle `SIGTERM`/`SIGINT` → graceful
   sequence, emergency halt path on fatal init error.
4. `config`: load minimal single-doc YAML from a known path (or cmdline override); expose as
   `Resource::MachineConfig` + a `Provider`.
5. `sequencer`: a Boot `PhaseList` that mounts fs → starts runtime → starts supervisor → runs one
   service; and a Shutdown sequence (stop services in reverse-dep order → unmount → halt).
6. `supervisor`: `ServiceManager` + `ServiceRunner` + `process` runner + `restart` wrapper; run one
   real `process` service (e.g. a long-lived dummy) reported via `ServiceStatus`.
7. **Acceptance:** in a QEMU/VM (or container with a fake-PID1 harness), machined-rs boots, mounts,
   brings up the runtime, starts the dummy service to Running, then performs a clean
   `SIGTERM`-driven shutdown. Covered by an integration test.

### Out of scope for the first spec
Real network/block/secrets controllers (M2), the gRPC API and `machinectl` (M3), containerd/CRI and
the actual rusternetes service (M4), upgrades/reset/kexec (M5).

## 6. Error handling & observability

- One `Error` enum per crate via `thiserror`; `anyhow` only at the binary boundary.
- Fatal init errors route to the **emergency** path (log to console + kernel ring buffer, optional
  reboot), never a bare panic in PID 1.
- `tracing` structured logs; service/runtime events recorded as resources so the future API can
  stream them. No log files on the SD during M0/M1 (write-amplification discipline) — console only.

## 7. Testing strategy

- **Unit:** `runtime-core` store/watch/controller semantics; sequencer phase ordering; supervisor
  state machine.
- **Integration:** a boot harness that runs the Boot→Shutdown sequence with a dummy service, asserting
  resource transitions. PID-1-specific behavior (mounts, signals) tested in a lightweight VM/container
  where privileged ops are available, gated behind a feature/CI job.
- **CI:** `make pre-commit` parity with rusternetes (fmt, clippy -D warnings, test). No Talos
  conformance to chase; correctness is defined by our own integration acceptance per milestone.

## 8. Key risks

- **PID 1 is unforgiving** — a crash is a kernel panic. Mitigate with the emergency path, exhaustive
  error handling at init, and VM-based integration tests early (M1).
- **netlink / block ops in Rust** (M2) — `rtnetlink`, `nix`, raw ioctls; spike early.
- **CRI/containerd from Rust** (M4) — the Rust containerd client is immature; plan to talk CRI gRPC
  directly via tonic if needed; spike before committing M4.
- **Footprint creep** — guard RSS and dependency weight as an ongoing constraint, not an afterthought.
