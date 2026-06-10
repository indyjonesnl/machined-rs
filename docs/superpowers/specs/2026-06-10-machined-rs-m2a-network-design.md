# machined-rs M2a — Network, Design

**Date:** 2026-06-10
**Status:** Approved (brainstorming) — proceeds to two implementation plans
**Parent design:** `2026-06-10-machined-rs-design.md` (this refines milestone M2's network portion)
**Builds on:** M0 (`runtime-core`) + M1 (boot slice), merged to `main`.

## 1. Overview

M2a makes a booted node configure its network from the machine config, using the M0 reconcile
runtime against real kernel state. It is the first milestone that registers actual `Controller`s
into the `Runtime` and the first to exercise the deferred owner-cascade teardown.

The original roadmap's M2 (network + block + time) is too large for one cycle. It decomposes into:
- **M2a — Network** (this spec): links, addresses, routes, hostname, `/etc/resolv.conf`.
- **M2b — Block**: disk discovery, partition, format, mount BOOT/STATE/EPHEMERAL.
- **M2c — Time**: time sync.

M2a is **static configuration only** — addresses/routes/hostname/DNS come from the machine config
(and kernel cmdline later). No DHCP, no operators/VIP, no live link monitoring. Those are later
milestones.

## 2. Goals / Non-goals

### Goals
- Extend the machine config with a typed `network` section.
- Add network resources (desired specs + observed status) to the closed `Resource` enum.
- Build **owner-cascade** into `runtime-core`: a reusable mechanism so a controller owns the
  resources it creates, garbage-collects ones no longer desired, and the finalizer protocol holds a
  desired resource alive until its consumer has reverted the real-world state it produced.
- Add a `netlink` crate: a `NetworkBackend` trait with an `rtnetlink` implementation and a fake.
- Add a `controllers` crate with the network controller pipeline (config → spec+status).
- Reconcile, on a real node, the configured links/addresses/routes/hostname/DNS into the kernel, and
  revert kernel state when config drops an item.

### Non-goals (deferred)
- DHCP / dynamic addressing / operators / VIP (later milestone).
- Live `rtnetlink` monitoring (status comes from each spec controller reporting what it applied, plus
  a read-back; an independent monitor controller is deferred).
- The full Talos `config → merge → spec → status` 4-layer pipeline; the **merge** layer is deferred
  until multiple config sources exist.
- nftables / firewall, KubeSpan/WireGuard, host DNS caching.
- Block and time subsystems (M2b, M2c).

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `resources` | + desired: `LinkSpec`, `AddressSpec`, `RouteSpec`, `HostnameSpec`, `ResolverSpec`; + observed: `LinkStatus`, `AddressStatus`, `RouteStatus`. New `ResourceType` variants. |
| `runtime-core` | **owner-cascade**: owner-stamping on create, `ReconcileCtx` helpers, and a generic `reconcile_owned` that diffs a desired set against owned resources and runs GC + finalizer-gated teardown. |
| `config` | + typed `network` section: interfaces (name, addresses, mtu, up), routes (to, via, link, metric), nameservers, search domains. |
| `netlink` (new leaf crate) | `NetworkBackend` trait + `RtNetlink` (real, `rtnetlink` crate) + `FakeNetworkBackend` (records calls, simulates kernel state for tests). Mirrors the `platform` trait/real/fake pattern. |
| `controllers` (new crate) | `network` module: the 6 controllers below. |
| `machined` | register the network controllers into the `Runtime` at boot, wiring the `NetworkBackend` + `Platform` + config `Provider`. |

Dependency direction: `netlink` is a leaf (depends only on `resources`/`common` for shared types).
`controllers` depends on `runtime-core`, `resources`, `config`, `netlink`, `platform`. `machined`
wires them.

### 3.2 Network resources

Desired (controller-owned, derived from config):
- `LinkSpec{ name, up, mtu }`
- `AddressSpec{ link, address /* IP+prefix */ }`
- `RouteSpec{ destination /* CIDR or default */, gateway, link, metric }`
- `HostnameSpec{ hostname }`
- `ResolverSpec{ nameservers, search }`

Observed (published by controllers after applying / reading back):
- `LinkStatus{ name, up, mtu, mac }`
- `AddressStatus{ link, address }`
- `RouteStatus{ destination, gateway, link }`

IDs are deterministic and stable (e.g. an address id is `link/addr/prefix`) so reconciliation is
idempotent and GC can identify orphans.

### 3.3 Controller pipeline (simplified COSI)

- **`NetworkConfigController`** — strong input `MachineConfig`; outputs (exclusive, owned) the desired
  `Link/Address/Route/Hostname/Resolver` specs. Translates the typed `network` config into the
  desired set and uses `reconcile_owned` so specs removed from config are torn down.
- **`LinkController`** — strong input `LinkSpec`; calls `NetworkBackend` to set admin up/down + MTU;
  publishes `LinkStatus`. Adds a finalizer to each `LinkSpec` it acts on; on `TearingDown` reverts
  (best-effort: links are not deleted, only returned to down/default MTU it set) and removes the
  finalizer.
- **`AddressController`** — strong input `AddressSpec`; add/del address via `NetworkBackend`; publishes
  `AddressStatus`; finalizer + revert-on-teardown (del address).
- **`RouteController`** — strong input `RouteSpec`; add/del route; publishes `RouteStatus`; finalizer +
  revert-on-teardown.
- **`HostnameController`** — strong input `HostnameSpec`; `platform.set_hostname`.
- **`ResolverController`** — strong input `ResolverSpec`; writes `/etc/resolv.conf` atomically.

Each spec controller is the single exclusive writer of its kernel domain and of its status resource.

### 3.4 owner-cascade in runtime-core

The deferred M0 work, built generically so every controller benefits:

- **Owner stamping:** `ReconcileCtx::create_owned(owner, obj)` / `update_owned` set
  `metadata.owner = Some(owner)`.
- **`reconcile_owned(ctx, owner, desired) -> Result<()>`:** given the full desired set this owner
  should have (keyed by id), it: creates/updates each desired resource (owner-stamped); and for each
  existing resource owned by `owner` but not in `desired`, calls `teardown` (not `destroy`) so any
  consumer finalizers gate removal; once a torn-down resource has no finalizers, it is destroyed.
- **Consumer protocol (helper):** a spec controller, in `reconcile`, iterates its strong-input
  resources: for those in `Running`, ensure-finalizer then apply; for those in `TearingDown`, revert
  then remove-finalizer. A `ReconcileCtx` helper (`reconcile_finalized`) encapsulates this loop so
  controllers supply only `apply`/`revert` closures.

This gives a two-level cascade: config-controller GC of desired specs, and spec-controller
finalizer-gated revert of the kernel state those specs produced. No automatic destruction of a
desired resource occurs until its real-world effect is reverted.

### 3.5 Wiring

`machined::run_daemon` builds the `Runtime`, constructs the `NetworkBackend` (real `RtNetlink` on
Linux, fake otherwise), registers the six controllers (passing `NetworkBackend`, `Platform`, and the
config `Provider`/`MachineConfig` resource), then runs the runtime alongside the boot sequence. The
controllers reconcile network continuously from the `MachineConfig` resource in the store.

## 4. Implementation plan decomposition

Two plans, each producing working/tested software:

- **M2a-1 — Foundation:** `runtime-core` owner-cascade (`create_owned`, `reconcile_owned`,
  `reconcile_finalized`) with unit tests; new network `Resource` types in `resources`; the `network`
  section in `config`. No netlink yet. Deliverable: the framework + types, fully unit-tested.
- **M2a-2 — Netlink + controllers:** the `netlink` crate (`NetworkBackend` + `RtNetlink` +
  `FakeNetworkBackend`); the `controllers` crate with the six controllers (unit-tested against the
  fake); `machined` wiring; the netns-gated integration test. Deliverable: a node that configures its
  network from config.

## 5. Error handling & observability

- One `Error` enum per new crate (`netlink`, `controllers`) via `thiserror`.
- Controllers never panic; a failed apply logs (via `tracing`) and is retried on the next reconcile
  (the runtime is level-triggered, so a transient netlink failure self-heals).
- Each applied change and each revert is logged with the resource id. Status resources make actual
  kernel state observable through the store (and, later, the API).

## 6. Testing strategy

- **Unit (no root):**
  - `runtime-core`: owner-stamping, `reconcile_owned` GC of removed-from-desired resources,
    finalizer-gated teardown ordering, `reconcile_finalized` apply/revert dispatch.
  - `config`: parsing the `network` section (valid, partial, empty).
  - controllers: config→spec translation; spec→`NetworkBackend` call translation and status
    publication, asserted against `FakeNetworkBackend` (which records calls and simulates kernel
    state); teardown reverts the right calls.
- **Integration (privileged, gated):** a test that runs inside a fresh network namespace
  (`unshare -rn` / a `CAP_NET_ADMIN` CI job) creating a dummy or veth link, drives the real
  `RtNetlink` backend to set it up, add an address and a route, and asserts via netlink read-back;
  then drops them and asserts removal. Gated behind a feature/CI job so the default `cargo test`
  stays root-free.
- **CI:** `make pre-commit` parity (fmt, clippy -D warnings, test) for the unit tier; the netns
  integration job runs separately.

## 7. Key risks

- **rtnetlink ergonomics / API drift** — spike the link/address/route CRUD + a netns round-trip
  early in M2a-2 before building all controllers.
- **owner-cascade correctness** — the finalizer/teardown ordering is subtle; cover it with focused
  runtime-core unit tests (resource removed from desired → torn down → consumer reverts → finalizer
  cleared → destroyed) before any controller depends on it. This is why it is its own first plan.
- **Privileged-test flakiness** — keep the netns test hermetic (fresh namespace per run, dummy links,
  no host interfaces) so it neither flakes nor touches the host network.
- **Idempotency under churn** — deterministic resource ids + level-triggered reconcile must make
  re-applying an already-correct address a no-op; assert this against the fake.
