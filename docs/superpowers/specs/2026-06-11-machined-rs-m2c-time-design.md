# machined-rs M2c — Time Sync, Design

**Date:** 2026-06-11
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (completes milestone M2: network + block + time)
**Builds on:** M0–M2b, merged to `main`.

## 1. Overview

M2c keeps the node clock synchronised via SNTP. It introduces a small, reusable **periodic-reconcile**
capability in `runtime-core` (the first controller that must re-run on a timer rather than only on
events), a pure-Rust SNTP client behind a `TimeSync` trait, and a `TimeSyncController` that queries
the configured servers, steps the clock when it is far off, and publishes `TimeStatus`. Completing
this finishes milestone M2.

## 2. Goals / Non-goals

### Goals
- Add `Controller::resync_interval() -> Option<Duration>` to `runtime-core` and wake each controller's
  reconcile loop on that timer (in addition to input events + the initial reconcile). Default `None`
  changes no existing controller.
- Add a `time` crate: a `TimeSync` trait with a pure-Rust SNTP `SntpTime` implementation
  (`clock_settime` via `nix`) and a `FakeTimeSync`.
- Add a `time { servers, disabled }` machine-config section.
- Add a `TimeStatus` resource.
- Add a `TimeSyncController` that re-syncs on the interval, steps the clock past a threshold, and
  publishes `TimeStatus`.

### Non-goals (deferred)
- **Clock slewing** (`adjtimex` gradual correction) — M2c steps only.
- NTP server mode (serving time), authentication (NTS), leap-second handling, the full NTP algorithm
  (filtering/clustering/multiple-sample statistics) — single-sample SNTP only.
- PTP, RTC management, timezone handling.
- Reacting to config changes at runtime (the config is read at controller construction; dynamic
  reconfig is a later concern, consistent with the other controllers).

## 3. Architecture

### 3.1 Crates

| crate | change |
|---|---|
| `runtime-core` | + `Controller::resync_interval(&self) -> Option<Duration>` (default `None`); the per-controller loop gains a timer arm in its `tokio::select!`. |
| `time` (new leaf crate) | `TimeSync` trait + `TimeOffset` + `SntpTime` (pure-Rust SNTP + `clock_settime`) + `FakeTimeSync` + pure `build_request`/`parse_offset`. Depends only on `tokio` (UDP) + `nix` (linux, clock) + `thiserror`/`tracing`. |
| `config` | + `TimeSection { servers: Vec<String>, disabled: bool }` on `MachineSection`. |
| `resources` | + `TimeStatus { synced, server, offset_ns, sync_count }`. |
| `controllers` | + `time::TimeSyncController`. |
| `machined` | register `TimeSyncController`. |

### 3.2 runtime-core: periodic reconcile

```rust
trait Controller {
    // ... existing: name, inputs, outputs, reconcile ...
    /// Optional periodic re-reconcile interval. Default: none (event-driven only).
    fn resync_interval(&self) -> Option<Duration> { None }
}
```

In `controller_loop`, when `resync_interval()` is `Some(d)`, build a `tokio::time::interval(d)` and add
a third `select!` arm that triggers `reconcile_once` on each tick (debounced/drained like the event
path). `None` controllers behave exactly as today (no timer arm). This is the only `runtime-core`
change and it is purely additive.

### 3.3 `time` crate

```text
TimeOffset = signed nanoseconds (i128) — the server time minus local time.

trait TimeSync (Send + Sync):
    async fn query_offset(&self, server: &str) -> Result<TimeOffset>  // one SNTP round-trip
    fn step_clock(&self, offset: TimeOffset) -> Result<()>            // set CLOCK_REALTIME += offset
```

- **`SntpTime`** (real): `query_offset` opens a tokio `UdpSocket`, sends `build_request()` to
  `server:123`, records T1 (send) and T4 (recv) with a recv timeout, and returns
  `parse_offset(response, T1, T4)`. `step_clock` reads the current `CLOCK_REALTIME`, adds the offset,
  and `clock_settime`s it (Linux; `nix`). Privileged — covered by a gated test.
- **`FakeTimeSync`**: returns a configured offset from `query_offset` (or an error for a configured
  "unreachable" server) and records each `step_clock` call.
- **Pure SNTP helpers** (unit-tested, no I/O):
  - `build_request() -> [u8; 48]` — LI=0, VN=4, mode=3 (first byte `0x23`), rest zero.
  - `parse_offset(resp: &[u8; 48], t1: SystemTime, t4: SystemTime) -> Result<TimeOffset>` — reads the
    server receive timestamp T2 (bytes 32–39) and transmit timestamp T3 (bytes 40–47) in NTP epoch
    (1900) format, converts to `SystemTime`, and computes `offset = ((T2−T1) + (T3−T4)) / 2`. Rejects
    a response with mode ≠ 4 or a zero transmit timestamp (Kiss-o'-Death / unsynced).

### 3.4 `TimeStatus` resource

```text
TimeStatus { synced: bool, server: String, offset_ns: i64, sync_count: u64 }
```

Namespace `runtime`. Singleton id `time`. Observed sync state.

### 3.5 `TimeSyncController`

- Inputs: none (resync is timer-driven).
- `resync_interval() -> Some(Duration::from_secs(11 * 60))` (≈11 min, the conventional minimum poll).
- Outputs: `TimeStatus` (Exclusive).
- Reconcile:
  - If `time.disabled` → publish `TimeStatus { synced: false, server: "", offset_ns: 0, sync_count }`
    and return.
  - Else try each configured server in order until one answers `query_offset`. On success: if
    `|offset| > 128 ms` → `step_clock(offset)`; increment `sync_count`; publish
    `TimeStatus { synced: true, server, offset_ns, sync_count }`.
  - If **no** server answers, log a warning and publish `TimeStatus { synced: false, ... }` (or leave
    the prior status); return `Ok` (transient — the next tick retries). Do **not** error the reconcile
    on unreachable servers (that would just spam the log; the timer already retries).
- Default servers when config omits them: a small fixed list (e.g. `0.pool.ntp.org`,
  `1.pool.ntp.org`).

### 3.6 Wiring

`machined::run_daemon` registers `TimeSyncController` (real `SntpTime` on Linux, `FakeTimeSync`
otherwise) alongside the other controllers. Its initial reconcile syncs at boot; the 11-min timer
re-syncs thereafter.

## 4. Error handling & observability

- `time` has a `TimeError` (`thiserror`): socket/IO, timeout, malformed response, clock-set failure.
- An unreachable server is a transient warning, not a controller error (the timer retries).
- A clock-set failure (e.g. not privileged) is logged; `TimeStatus.synced` reflects whether a sync
  actually applied.
- `TimeStatus` (offset, server, sync_count) makes sync state observable through the store.

## 5. Testing strategy

- **Unit (root-free):**
  - `build_request` — correct first byte (`0x23`), length 48.
  - `parse_offset` — a synthetic 48-byte response with known T2/T3 and given T1/T4 yields the expected
    offset; a mode-≠4 or zero-transmit response is rejected.
  - `runtime-core` periodic: a controller with `resync_interval = Some(20ms)` reconciles ≥2 times
    within ~100ms (proves the timer arm fires) while a `None` controller reconciles once.
  - `FakeTimeSync` — returns the configured offset; records steps; an "unreachable" server errors.
  - `TimeSyncController` against `FakeTimeSync`: offset > threshold → `step_clock` recorded +
    `TimeStatus{synced:true}`; offset < threshold → no step but still `synced:true`; `disabled` → no
    query, `TimeStatus{synced:false}`; all-servers-unreachable → `Ok` + `synced:false`.
- **Integration (root-free):** a **loopback fake NTP server** — bind a UDP socket, reply to a query
  with a crafted response, and drive the real `SntpTime::query_offset` against it, asserting the
  computed offset. No root needed (UDP loopback).
- **Integration (privileged, gated):** a `clock_settime` round-trip — read the clock, `step_clock` a
  tiny offset, confirm it moved, restore. `#[ignore]` (needs `CAP_SYS_TIME`).
- **CI:** `make pre-commit` for the unit + loopback tier; the privileged clock test runs separately.

## 6. Key risks

- **NTP timestamp arithmetic** — NTP uses a 1900 epoch and 32.32 fixed-point; the era/overflow and
  unsigned-fraction handling must be right. Cover `parse_offset` with a fixture whose expected offset
  is computed by hand, and the loopback test with a server-controlled timestamp.
- **`resync_interval` correctness** — the timer arm must not starve the event path nor busy-loop;
  `tokio::time::interval` ticks are coalesced and the first tick is immediate (skip or consume it so
  the initial reconcile isn't double-fired). The periodic unit test pins the firing behavior.
- **Clock-step safety** — stepping `CLOCK_REALTIME` backward can disturb timers; for M2c (boot-time +
  infrequent) this is acceptable, and the 128 ms threshold avoids churn. Slewing is the deferred
  refinement.
- **UDP query hangs** — `query_offset` must use a bounded recv timeout so an unreachable server fails
  fast and the controller moves to the next one.
