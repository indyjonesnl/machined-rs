# machined-rs M3c — Action RPCs, Design

**Date:** 2026-06-11
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (extends milestone M3: management API)
**Builds on:** M3a (PKI + mTLS gRPC API + machinectl), merged to `main`.

## 1. Overview

M3c lets an authenticated operator **act** on the node, not just read it: `Reboot` and `Shutdown`
RPCs that drive the node through its existing graceful shutdown sequence and then `reboot(2)` /
`poweroff(2)`. The handler hands a `NodeAction` to the daemon's main loop over a channel and returns
immediately; the daemon performs the same `shutdown_sequence` the OS signal path already uses, with a
final reboot/poweroff appended. Completes the read+act management surface.

## 2. Goals / Non-goals

### Goals
- Add `Reboot` and `Shutdown` RPCs to `machine.proto`.
- Add a `NodeAction` enum and an `mpsc::Sender<NodeAction>` to the `apiserver` `Machine` service; the
  handlers send the action and return `Ok`.
- Wire the action channel into `machined::run_daemon`: the post-boot wait selects on the OS signal
  **or** an action; after `shutdown_sequence` + runtime stop, it calls `platform.reboot()` /
  `platform.poweroff()` for the chosen final action.
- Add `machinectl reboot` + `machinectl shutdown` subcommands.

### Non-goals (deferred)
- **Reset** (wipe STATE + reprovision) and **upgrade/kexec** — the destructive M5 lifecycle work.
- **Authorization/RBAC** — any mTLS-authenticated (CA-signed) client may act; role separation
  (admin vs read-only certs) is a later authz milestone.
- A confirmation/lock or "are you sure" gate; `--wait`/progress streaming; staged drain (cordon) —
  later.
- Graceful shutdown of the API server task itself (carried forward from M3a-2; an M5 refinement).

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `apiserver` | `proto/machine.proto` + `Reboot`/`Shutdown` RPCs; `NodeAction` enum; `Machine` gains an `mpsc::Sender<NodeAction>`; `Machine::new` + `serve` gain the sender param; handlers send + return `Empty`. |
| `machined` | build the action channel; pass the sender to `serve`; the post-boot wait becomes a `select!` over the OS signal and the action receiver; a `FinalAction` (`Stop`/`Reboot`/`Poweroff`) chosen there drives `platform.reboot()`/`poweroff()` after the shutdown sequence. |
| `machinectl` | `reboot` + `shutdown` subcommands calling the new RPCs. |

### 3.2 `apiserver`: NodeAction + handlers

```proto
service MachineService {
  rpc Version(Empty) returns (VersionResponse);
  rpc ListResources(ListResourcesRequest) returns (ListResourcesResponse);
  rpc Reboot(Empty) returns (Empty);
  rpc Shutdown(Empty) returns (Empty);
}
```

```text
pub enum NodeAction { Reboot, Shutdown }

struct Machine { state: State, version: String, actions: mpsc::Sender<NodeAction> }
Machine::new(state, version, actions) -> Machine
```

- `reboot` handler: `actions.send(NodeAction::Reboot).await` → `Ok(Empty)`; a closed channel →
  `Status::unavailable`.
- `shutdown` handler: same with `NodeAction::Shutdown`.
- The handler returns **before** the node goes down ("accepted"); the client's RPC succeeds and the
  connection then drops as the daemon shuts down.
- `serve(addr, state, version, pki, actions)` threads the sender into the `Machine`.

The existing read-only call sites (`grpc.rs` tests, `machinectl` e2e) construct `Machine`/`serve` with
a throwaway `mpsc::channel(1)` sender (they never invoke actions).

### 3.3 `machined`: the graceful action path

`run_daemon` today: `boot → wait_for_termination() → shutdown_sequence → cancel runtime → return`.

M3c:
- Build `let (action_tx, mut action_rx) = mpsc::channel::<NodeAction>(1);` before spawning the server;
  pass `action_tx` to `serve`.
- Replace `wait_for_termination().await` with:

```text
let final_action = tokio::select! {
    _ = pid1::wait_for_termination() => FinalAction::Stop,
    a = action_rx.recv()             => match a {
        Some(NodeAction::Reboot)   => FinalAction::Reboot,
        Some(NodeAction::Shutdown) => FinalAction::Poweroff,
        None                       => FinalAction::Stop,   // senders dropped
    },
};
```

- After `shutdown_sequence` + `shutdown.cancel()` + `rt_handle.await`, perform the final action:

```text
match final_action {
    FinalAction::Stop     => {}                       // signal-driven stop (existing behavior)
    FinalAction::Reboot   => platform.reboot()?,      // reboot(2)
    FinalAction::Poweroff => platform.poweroff()?,    // poweroff(2)
}
```

`FinalAction` is a small `machined`-local enum. `platform.reboot()/poweroff()` already exist
(`LinuxPlatform` real, `FakePlatform` records). On the real platform the call does not return; the
`?` only matters on the fake/error path.

### 3.4 `machinectl`

Two new `clap` subcommands:
- `machinectl reboot` → `Reboot` RPC; prints `reboot requested`.
- `machinectl shutdown` → `Shutdown` RPC; prints `shutdown requested`.

Both reuse the existing mTLS `connect()`.

## 4. Error handling & observability

- A closed action channel (daemon tearing down) → `Status::unavailable` to the client.
- The handler logs (via `tracing`) the requested action before sending.
- The daemon logs the chosen `FinalAction` before executing it.
- A `platform.reboot()/poweroff()` error (only reachable on the fake / unprivileged) is logged; on a
  real node the syscall does not return.

## 5. Testing strategy

- **Unit (root-free):**
  - `machinectl` — `reboot`/`shutdown` subcommands parse (extend the clap tests).
- **Integration (root-free):**
  - `apiserver` — stand up a server (plaintext, like the existing `grpc.rs` tests) with a held
    `action_rx`; call `Reboot` → handler returns `Ok` **and** `action_rx` receives
    `NodeAction::Reboot`; call `Shutdown` → `NodeAction::Shutdown`. (The actual reboot is not performed
    — the handler only enqueues.)
  - `machinectl` e2e — extend the existing e2e: run `machinectl reboot` against the real mTLS server
    and assert the server's `action_rx` receives `NodeAction::Reboot` (proving CLI→mTLS→handler→channel
    end-to-end).
- **`machined` wiring** — build-smoke (`cargo run -p machined -- version`); the main-loop `select!` +
  `FinalAction` + `platform` call are wiring, validated by inspection + the fake-platform path (the
  real reboot is privileged, exercised manually), consistent with how the other `run_daemon` wiring is
  treated.
- **CI:** `make pre-commit`.

## 6. Key risks

- **`Machine::new`/`serve` signature change** breaks every existing call site (3 in `grpc.rs`, 1 in the
  `machinectl` e2e, 1 in `machined`). Each must add the sender arg; read-only sites use a throwaway
  channel. A missed site is a compile error.
- **Channel capacity / lost action** — capacity 1 is enough (one action ends the node); a second
  concurrent action while one is in flight may be dropped (`try_send` would error) — acceptable, the
  node is already going down. Use `send().await` (awaits capacity) in the handler; the daemon receives
  exactly one then proceeds to shut down.
- **Ordering** — the handler must return its `Ok` to the client *before* the daemon kills the process,
  or the client sees a transport error instead of success. Because the daemon only acts *after*
  `recv()` + the full `shutdown_sequence`, the handler's response is already flushed; the realistic
  risk is negligible, but the e2e asserts the channel delivery (not the client seeing `Ok` under a
  real reboot, which can't run in tests).
- **Don't bypass the graceful path** — the handler must only enqueue; never call `platform.reboot()`
  directly (that would skip `shutdown_sequence` and tear down mid-RPC).
