# machined-rs M2c — Time Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M0–M2b merged to `main`. Work on branch `spec/machined-rs-m2c-time`.

**Goal:** Keep the node clock synced via SNTP: a reusable periodic-reconcile capability in `runtime-core`, a pure-Rust SNTP client behind a `TimeSync` trait, and a `TimeSyncController` that re-syncs every ~11 min and steps the clock when it is far off. Completes milestone M2.

**Architecture:** `runtime-core` gains `Controller::resync_interval() -> Option<Duration>` (default `None`); the loop wakes on that timer too. A `time` crate provides pure SNTP `build_request`/`parse_offset` helpers, a `TimeSync` trait, a real `SntpTime` (UDP query + `clock_settime`), and `FakeTimeSync`. The controller queries the configured servers, steps the clock past a 128 ms threshold, and publishes `TimeStatus`.

**Tech Stack:** `tokio` (UDP), `nix` (linux clock), `thiserror`, plus the existing stack. Pure `std::time` for the SNTP math.

---

## File Structure

```
crates/runtime-core/src/runtime.rs    # MODIFY: Controller::resync_interval + loop timer arm
crates/resources/src/time.rs          # NEW: TimeStatus
crates/resources/src/{metadata,resource,lib}.rs   # MODIFY: 1 variant + re-export
crates/config/src/{types,provider,lib}.rs         # MODIFY: TimeSection + time()
crates/time/
├── Cargo.toml                         # NEW
└── src/
    ├── lib.rs                         # NEW: TimeOffset, TimeError, TimeSync trait, re-exports
    ├── sntp.rs                        # NEW: build_request + parse_offset (pure)
    ├── real.rs                        # NEW: SntpTime (UDP + clock_settime)
    └── fake.rs                        # NEW: FakeTimeSync
crates/time/tests/loopback_ntp.rs      # NEW: root-free loopback fake-NTP-server + gated clock test
crates/controllers/src/time/mod.rs     # NEW
crates/controllers/src/time/sync.rs    # NEW: TimeSyncController
crates/controllers/src/lib.rs          # MODIFY: pub mod time
crates/machined/src/main.rs            # MODIFY: register TimeSyncController
crates/machined/tests/time.rs          # NEW: e2e against fake
```

---

## Task 1: `runtime-core` periodic reconcile

**Files:**
- Modify: `crates/runtime-core/src/runtime.rs`

- [ ] **Step 1: Add the trait default method**

In `crates/runtime-core/src/runtime.rs`, add to the `Controller` trait (after `reconcile`):

```rust
    /// Optional periodic re-reconcile interval. Default `None` = event-driven only.
    fn resync_interval(&self) -> Option<std::time::Duration> {
        None
    }
```

- [ ] **Step 2: Add the timer arm to the controller loop**

In `controller_loop`, after `let mut rx = state.watch();` and before the initial reconcile, build the
optional interval (first tick at `now + d`, so it does not double-fire the initial reconcile):

```rust
    let mut resync = controller.resync_interval().map(|d| {
        let mut iv = tokio::time::interval_at(tokio::time::Instant::now() + d, d);
        // Skip missed ticks so a slow reconcile cannot cause a catch-up burst.
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        iv
    });
```

Add a `select!` arm for it. Replace the existing `loop { tokio::select! { ... } }` body so it includes
the timer arm:

```rust
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                return;
            }
            _ = tick(resync.as_mut()) => {
                reconcile_once(&mut controller, &ctx).await;
            }
            recv = rx.recv() => {
                match recv {
                    Ok(event) => {
                        if !matches_inputs(&inputs, &event) {
                            continue;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(controller = controller.name(), skipped = n, "watch lagged; forcing reconcile");
                    }
                    Err(RecvError::Closed) => return,
                }
                // Debounce: collapse a burst into a single reconcile.
                tokio::time::sleep(DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
                reconcile_once(&mut controller, &ctx).await;
            }
        }
    }
```

Add this free helper near `matches_inputs`:

```rust
/// Await the next tick of an optional interval; never resolves when `None`,
/// so a controller with no resync interval has no timer arm.
async fn tick(interval: Option<&mut tokio::time::Interval>) {
    match interval {
        Some(iv) => {
            iv.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}
```

- [ ] **Step 3: Write the periodic test**

Add to the `tests` module in `runtime.rs`:

```rust
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counter {
        count: Arc<AtomicUsize>,
        interval: Option<Duration>,
    }

    #[async_trait]
    impl Controller for Counter {
        fn name(&self) -> &str {
            "counter"
        }
        fn inputs(&self) -> Vec<Input> {
            Vec::new()
        }
        fn outputs(&self) -> Vec<Output> {
            Vec::new()
        }
        fn resync_interval(&self) -> Option<Duration> {
            self.interval
        }
        async fn reconcile(&mut self, _ctx: &ReconcileCtx) -> Result<()> {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn periodic_controller_reconciles_repeatedly() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut rt = Runtime::new();
        rt.register(Box::new(Counter {
            count: count.clone(),
            interval: Some(Duration::from_millis(20)),
        }));
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let handle = tokio::spawn(async move { rt.run(token).await });

        tokio::time::sleep(Duration::from_millis(120)).await;
        shutdown.cancel();
        handle.await.unwrap().unwrap();

        // Initial reconcile + several timer ticks.
        assert!(
            count.load(Ordering::SeqCst) >= 3,
            "expected periodic reconciles, got {}",
            count.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn non_periodic_controller_reconciles_once() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut rt = Runtime::new();
        rt.register(Box::new(Counter {
            count: count.clone(),
            interval: None,
        }));
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let handle = tokio::spawn(async move { rt.run(token).await });

        tokio::time::sleep(Duration::from_millis(120)).await;
        shutdown.cancel();
        handle.await.unwrap().unwrap();

        // Only the initial reconcile (no inputs, no timer).
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
```

> The `tests` module imports `Duration`, `async_trait`, `Controller`, `Input`, `Output`,
> `ReconcileCtx`, `Result`, `Runtime`, `CancellationToken` (from the existing
> `controller_reacts_to_input_change` test). Add `use std::sync::Arc;` AND
> `use std::sync::atomic::{AtomicUsize, Ordering};` (the existing tests do not import `Arc`).

- [ ] **Step 4: Test + clippy + commit**

Run: `cargo test -p machined-runtime-core` → existing + the two periodic tests pass.
Run: `cargo build --workspace` → PASS (the new trait method has a default, so no existing controller breaks).
Run: `cargo clippy --all-targets --all-features -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/runtime-core
git commit -m "feat(runtime-core): periodic reconcile via Controller::resync_interval"
```

---

## Task 2: `TimeStatus` resource + `time` config section

**Files:**
- Create: `crates/resources/src/time.rs`
- Modify: `crates/resources/src/{metadata,resource,lib}.rs`
- Modify: `crates/config/src/{types,provider,lib}.rs`
- Modify: the four `MachineSection { ... }` literals (the new config field breaks them)

- [ ] **Step 1: Add the ResourceType variant + TimeStatus**

In `crates/resources/src/metadata.rs`, add `TimeStatus` after `MountStatus` in the enum + Display:

```rust
    MountStatus,
    TimeStatus,
}
```

```rust
            ResourceType::MountStatus => "MountStatus",
            ResourceType::TimeStatus => "TimeStatus",
        };
```

Create `crates/resources/src/time.rs`:

```rust
//! Time-sync resource (observed state). Pure data.

/// Observed clock-sync state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeStatus {
    pub synced: bool,
    pub server: String,
    pub offset_ns: i64,
    pub sync_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs() {
        let t = TimeStatus {
            synced: true,
            server: "0.pool.ntp.org".into(),
            offset_ns: -1234,
            sync_count: 1,
        };
        assert!(t.synced);
    }
}
```

- [ ] **Step 2: Add the Resource variant + module + re-export**

In `crates/resources/src/resource.rs`, add the import + variant + `resource_type` arm:

```rust
use crate::time::TimeStatus;
```

```rust
    MountStatus(MountStatus),
    TimeStatus(TimeStatus),
}
```

```rust
            Resource::MountStatus(_) => ResourceType::MountStatus,
            Resource::TimeStatus(_) => ResourceType::TimeStatus,
        }
```

In `crates/resources/src/lib.rs`, add the module + re-export:

```rust
pub mod time;
```

```rust
pub use time::TimeStatus;
```

(Add `pub mod time;` alongside the other `pub mod` lines, and the `pub use` alongside the others.)

- [ ] **Step 3: Add the config `time` section**

In `crates/config/src/types.rs`, add a `#[serde(default)] time` field to `MachineSection` (after
`install`):

```rust
    /// Time-sync configuration.
    #[serde(default)]
    pub time: TimeSection,
```

Append the type:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSection {
    /// NTP servers to query, in order. Empty → the controller's default pool.
    #[serde(default)]
    pub servers: Vec<String>,
    /// Disable time sync entirely.
    #[serde(default)]
    pub disabled: bool,
}
```

In `crates/config/src/provider.rs`, add `TimeSection` to the import and an accessor:

```rust
use crate::types::{InstallSection, MachineConfig, NetworkSection, ServiceConfig, Sysctl, TimeSection};
```

```rust
    pub fn time(&self) -> &TimeSection {
        &self.config.machine.time
    }
```

In `crates/config/src/lib.rs`, add `TimeSection` to the `types` re-export.

- [ ] **Step 4: Add a failing config parse test**

Append to the `tests` module in `crates/config/src/load.rs`:

```rust
    #[test]
    fn parses_time_section() {
        let cfg = load_from_str("machine:\n  time:\n    servers: [a.ntp, b.ntp]\n    disabled: true\n")
            .unwrap();
        assert_eq!(cfg.machine.time.servers, vec!["a.ntp", "b.ntp"]);
        assert!(cfg.machine.time.disabled);
    }

    #[test]
    fn time_defaults_empty_enabled() {
        let cfg = load_from_str("machine: {}").unwrap();
        assert!(cfg.machine.time.servers.is_empty());
        assert!(!cfg.machine.time.disabled);
    }
```

- [ ] **Step 5: Update the four `MachineSection { ... }` literals**

Adding `time` makes the explicit literals non-exhaustive (E0063). Add `time: Default::default(),`
(after the `install:` field) to the `MachineSection { ... }` literal in: `crates/sequencer/src/boot.rs`,
`crates/machined/tests/boot_harness.rs`, `crates/machined/tests/network.rs`, and
`crates/controllers/src/network/config_controller.rs` (the `provider()` helper).

- [ ] **Step 6: Test + clippy + commit**

Run: `cargo test -p machined-resources` → existing + `time::tests::constructs` pass.
Run: `cargo test -p machined-config` → existing + 2 time tests pass.
Run: `cargo build --workspace` → PASS (literals updated).
Run: `cargo clippy --all-targets --all-features -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/resources crates/config crates/sequencer crates/controllers crates/machined
git commit -m "feat(resources,config): TimeStatus + time config section"
```

---

## Task 3: `time` crate — SNTP + TimeSync

**Files:**
- Modify: `Cargo.toml` (members + deps)
- Create: `crates/time/Cargo.toml`
- Create: `crates/time/src/lib.rs`
- Create: `crates/time/src/sntp.rs`
- Create: `crates/time/src/fake.rs`
- Create: `crates/time/src/real.rs`
- Create: `crates/time/tests/loopback_ntp.rs`

- [ ] **Step 1: Add the crate to the workspace**

In root `Cargo.toml`, add `"crates/time"` to `members` and add `machined-time = { path = "crates/time" }`
to `[workspace.dependencies]`.

- [ ] **Step 2: Create the manifest**

Create `crates/time/Cargo.toml`:

```toml
[package]
name = "machined-time"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
async-trait.workspace = true
thiserror.workspace = true
tracing.workspace = true
tokio.workspace = true

[target.'cfg(target_os = "linux")'.dependencies]
nix.workspace = true

[dev-dependencies]
tokio = { workspace = true }
```

- [ ] **Step 3: Write the pure SNTP helpers with tests**

Create `crates/time/src/sntp.rs`:

```rust
//! Pure SNTP packet build + offset computation. No I/O.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{TimeError, TimeOffset};

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const NTP_UNIX_OFFSET: i128 = 2_208_988_800;

/// Build a 48-byte SNTP client request: LI=0, VN=4, Mode=3 (`0x23`), rest zero.
pub fn build_request() -> [u8; 48] {
    let mut p = [0u8; 48];
    p[0] = 0x23;
    p
}

/// Convert an 8-byte NTP timestamp (32.32 fixed point, 1900 epoch) to
/// nanoseconds since the Unix epoch.
fn ntp_to_unix_ns(b: &[u8]) -> i128 {
    let secs = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i128;
    let frac = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as i128;
    let frac_ns = (frac * 1_000_000_000) >> 32;
    (secs - NTP_UNIX_OFFSET) * 1_000_000_000 + frac_ns
}

fn st_to_ns(t: SystemTime) -> i128 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Compute the clock offset from an SNTP response and the local send (T1) and
/// receive (T4) times: `offset = ((T2 - T1) + (T3 - T4)) / 2`.
/// Rejects a non-server response (mode != 4) or a zero transmit timestamp.
pub fn parse_offset(resp: &[u8; 48], t1: SystemTime, t4: SystemTime) -> Result<TimeOffset, TimeError> {
    if resp[0] & 0x07 != 4 {
        return Err(TimeError::BadResponse("not a server (mode != 4) response".into()));
    }
    if resp[40..48].iter().all(|&x| x == 0) {
        return Err(TimeError::BadResponse("zero transmit timestamp".into()));
    }
    let t1n = st_to_ns(t1);
    let t4n = st_to_ns(t4);
    let t2n = ntp_to_unix_ns(&resp[32..40]);
    let t3n = ntp_to_unix_ns(&resp[40..48]);
    Ok(((t2n - t1n) + (t3n - t4n)) / 2)
}

/// Encode nanoseconds-since-Unix-epoch into an 8-byte NTP timestamp (test/server helper).
pub fn unix_ns_to_ntp(ns: i128) -> [u8; 8] {
    let secs = (ns.div_euclid(1_000_000_000) + NTP_UNIX_OFFSET) as u32;
    let frac = ((ns.rem_euclid(1_000_000_000) << 32) / 1_000_000_000) as u32;
    let mut b = [0u8; 8];
    b[0..4].copy_from_slice(&secs.to_be_bytes());
    b[4..8].copy_from_slice(&frac.to_be_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_well_formed() {
        let r = build_request();
        assert_eq!(r.len(), 48);
        assert_eq!(r[0], 0x23);
    }

    #[test]
    fn computes_offset() {
        // T1 = T4 = Unix epoch; server T2 = T3 = Unix 1000s → offset = +1000s.
        let mut resp = [0u8; 48];
        resp[0] = 0x24; // mode 4
        let ts = unix_ns_to_ntp(1000 * 1_000_000_000);
        resp[32..40].copy_from_slice(&ts);
        resp[40..48].copy_from_slice(&ts);
        let off = parse_offset(&resp, UNIX_EPOCH, UNIX_EPOCH).unwrap();
        assert_eq!(off, 1000 * 1_000_000_000);
    }

    #[test]
    fn rejects_non_server_and_zero_transmit() {
        let mut resp = [0u8; 48];
        resp[0] = 0x1b; // mode 3 (client) — not a server response
        let ts = unix_ns_to_ntp(0);
        resp[32..40].copy_from_slice(&ts);
        resp[40..48].copy_from_slice(&ts);
        assert!(parse_offset(&resp, UNIX_EPOCH, UNIX_EPOCH).is_err());

        let mut z = [0u8; 48];
        z[0] = 0x24;
        // transmit timestamp left zero → rejected
        assert!(parse_offset(&z, UNIX_EPOCH, UNIX_EPOCH).is_err());
    }
}
```

- [ ] **Step 4: Write the trait + crate root + fake**

Create `crates/time/src/lib.rs`:

```rust
//! Time synchronisation: a `TimeSync` trait with a pure-Rust SNTP `SntpTime`
//! implementation and an in-memory fake.

pub mod fake;
pub mod real;
pub mod sntp;

use async_trait::async_trait;

pub use fake::FakeTimeSync;
pub use real::SntpTime;
pub use sntp::{build_request, parse_offset};

/// Clock offset (server minus local) in signed nanoseconds.
pub type TimeOffset = i128;

#[derive(thiserror::Error, Debug)]
pub enum TimeError {
    #[error("time io: {0}")]
    Io(String),
    #[error("time query timed out")]
    Timeout,
    #[error("bad ntp response: {0}")]
    BadResponse(String),
    #[error("clock set: {0}")]
    ClockSet(String),
}

pub type Result<T> = std::result::Result<T, TimeError>;

/// Query an NTP server for the clock offset and step the system clock.
#[async_trait]
pub trait TimeSync: Send + Sync {
    /// One SNTP round-trip against `addr` (`"host:port"`). Returns the offset.
    async fn query_offset(&self, addr: &str) -> Result<TimeOffset>;
    /// Step `CLOCK_REALTIME` by `offset`.
    fn step_clock(&self, offset: TimeOffset) -> Result<()>;
}
```

Create `crates/time/src/fake.rs`:

```rust
//! In-memory `TimeSync` for root-free tests.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::{Result, TimeError, TimeOffset, TimeSync};

#[derive(Default)]
struct FakeState {
    /// Per-addr canned offset; absent → query errors (unreachable).
    offsets: HashMap<String, TimeOffset>,
    /// Recorded step_clock calls.
    steps: Vec<TimeOffset>,
}

#[derive(Default)]
pub struct FakeTimeSync {
    state: Mutex<FakeState>,
}

impl FakeTimeSync {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make `addr` answer with `offset`.
    pub fn with_offset(self, addr: &str, offset: TimeOffset) -> Self {
        self.state.lock().unwrap().offsets.insert(addr.to_string(), offset);
        self
    }

    /// Recorded step_clock offsets.
    pub fn steps(&self) -> Vec<TimeOffset> {
        self.state.lock().unwrap().steps.clone()
    }
}

#[async_trait]
impl TimeSync for FakeTimeSync {
    async fn query_offset(&self, addr: &str) -> Result<TimeOffset> {
        self.state
            .lock()
            .unwrap()
            .offsets
            .get(addr)
            .copied()
            .ok_or_else(|| TimeError::Io(format!("unreachable: {addr}")))
    }

    fn step_clock(&self, offset: TimeOffset) -> Result<()> {
        self.state.lock().unwrap().steps.push(offset);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_configured_addr_and_records_steps() {
        let f = FakeTimeSync::new().with_offset("a:123", 5_000);
        assert_eq!(f.query_offset("a:123").await.unwrap(), 5_000);
        assert!(f.query_offset("b:123").await.is_err());
        f.step_clock(5_000).unwrap();
        assert_eq!(f.steps(), vec![5_000]);
    }
}
```

- [ ] **Step 5: Write the real `SntpTime` (SPIKE: nix clock + UDP)**

> **SPIKE NOTE:** the `nix` clock API (`nix::time::{clock_gettime, clock_settime, ClockId}` +
> `nix::sys::time::TimeSpec`) and the `tokio::net::UdpSocket` calls are best-effort. If a `nix` time
> symbol differs in 0.29, adapt inside `real.rs` (the operation — read/set `CLOCK_REALTIME` — is
> stable); the `nix` `time` feature may need enabling in the workspace `nix` features (note it). Do NOT
> change the `TimeSync` trait. The loopback test (Step 6) is the acceptance criterion for the UDP path.

Create `crates/time/src/real.rs`:

```rust
//! Real `TimeSync`: SNTP over UDP + `clock_settime`.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::sntp::{build_request, parse_offset};
use crate::{Result, TimeError, TimeOffset, TimeSync};

pub struct SntpTime;

impl SntpTime {
    pub fn new() -> Self {
        SntpTime
    }
}

impl Default for SntpTime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TimeSync for SntpTime {
    async fn query_offset(&self, addr: &str) -> Result<TimeOffset> {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| TimeError::Io(e.to_string()))?;
        socket
            .connect(addr)
            .await
            .map_err(|e| TimeError::Io(e.to_string()))?;

        let req = build_request();
        let t1 = SystemTime::now();
        socket.send(&req).await.map_err(|e| TimeError::Io(e.to_string()))?;

        let mut resp = [0u8; 48];
        let n = tokio::time::timeout(Duration::from_secs(3), socket.recv(&mut resp))
            .await
            .map_err(|_| TimeError::Timeout)?
            .map_err(|e| TimeError::Io(e.to_string()))?;
        let t4 = SystemTime::now();
        if n < 48 {
            return Err(TimeError::BadResponse("short response".into()));
        }
        parse_offset(&resp, t1, t4)
    }

    fn step_clock(&self, offset: TimeOffset) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use nix::sys::time::TimeSpec;
            use nix::time::{clock_gettime, clock_settime, ClockId};

            let now = clock_gettime(ClockId::CLOCK_REALTIME)
                .map_err(|e| TimeError::ClockSet(e.to_string()))?;
            let now_ns = now.tv_sec() as i128 * 1_000_000_000 + now.tv_nsec() as i128;
            let new_ns = now_ns + offset;
            let secs = new_ns.div_euclid(1_000_000_000) as i64;
            let nsecs = new_ns.rem_euclid(1_000_000_000) as i64;
            clock_settime(ClockId::CLOCK_REALTIME, TimeSpec::new(secs, nsecs))
                .map_err(|e| TimeError::ClockSet(e.to_string()))?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = offset;
            Err(TimeError::ClockSet("clock_settime unsupported on this platform".into()))
        }
    }
}
```

- [ ] **Step 6: Write the loopback (root-free) + gated clock tests**

Create `crates/time/tests/loopback_ntp.rs`:

```rust
//! Loopback fake-NTP-server integration (root-free) + a gated clock_settime test.

use std::time::{SystemTime, UNIX_EPOCH};

use machined_time::sntp::unix_ns_to_ntp;
use machined_time::{SntpTime, TimeSync};

#[tokio::test]
async fn queries_loopback_ntp_server() {
    // A fake NTP server: reply to one request with T2=T3 set to "now + 5s".
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap().to_string();

    let server_task = tokio::spawn(async move {
        let mut buf = [0u8; 48];
        let (_, peer) = server.recv_from(&mut buf).await.unwrap();
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i128;
        let ts = unix_ns_to_ntp(now_ns + 5 * 1_000_000_000);
        let mut resp = [0u8; 48];
        resp[0] = 0x24; // mode 4
        resp[32..40].copy_from_slice(&ts);
        resp[40..48].copy_from_slice(&ts);
        server.send_to(&resp, peer).await.unwrap();
    });

    let offset = SntpTime::new().query_offset(&addr).await.unwrap();
    server_task.await.unwrap();

    // The server claimed ~+5s; allow generous slack for round-trip/scheduling.
    let secs = offset as f64 / 1e9;
    assert!(secs > 4.0 && secs < 6.0, "offset {secs}s not ~5s");
}

#[tokio::test]
#[ignore = "requires CAP_SYS_TIME (clock_settime)"]
async fn steps_clock() {
    let t = SntpTime::new();
    let before = SystemTime::now();
    t.step_clock(2 * 1_000_000_000).unwrap(); // +2s
    let after = SystemTime::now();
    let delta = after.duration_since(before).unwrap().as_secs_f64();
    // Restore.
    t.step_clock(-2 * 1_000_000_000).unwrap();
    assert!(delta > 1.5, "clock should have jumped ~2s, moved {delta}s");
}
```

- [ ] **Step 7: Build + test + commit**

Run: `cargo test -p machined-time` → sntp (3) + fake (1) + `queries_loopback_ntp_server` pass; `steps_clock` ignored. (If `nix` time symbols differ, adapt per the SPIKE NOTE; add the `nix` `time` feature to the workspace `nix` features if needed and note it.)
Run: `cargo clippy -p machined-time --all-targets -- -D warnings` → clean.
Run: `cargo build --workspace` → PASS.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add Cargo.toml Cargo.lock crates/time
git commit -m "feat(time): SNTP TimeSync trait + SntpTime + fake"
```

---

## Task 4: `TimeSyncController`

**Files:**
- Create: `crates/controllers/src/time/mod.rs`
- Create: `crates/controllers/src/time/sync.rs`
- Modify: `crates/controllers/src/lib.rs`
- Modify: `crates/controllers/Cargo.toml` (add machined-time dep)

- [ ] **Step 1: Add the time dep**

In `crates/controllers/Cargo.toml` `[dependencies]`, add `machined-time.workspace = true`.

- [ ] **Step 2: Write the controller**

Create `crates/controllers/src/time/mod.rs`:

```rust
//! Time controllers.

pub mod sync;

pub use sync::TimeSyncController;

use std::fmt::Display;

use machined_runtime_core::Error;

/// Namespace for time resources.
pub const NS: &str = "runtime";

pub(crate) fn ctl<E: Display>(e: E) -> Error {
    Error::Controller(e.to_string())
}
```

Create `crates/controllers/src/time/sync.rs`:

```rust
//! Periodically syncs the clock via SNTP and publishes TimeStatus.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use machined_config::Provider;
use machined_resources::{Resource, ResourceObject, ResourceType, TimeStatus};
use machined_runtime_core::{reconcile_owned, Controller, Input, Output, OutputKind, ReconcileCtx};
use machined_time::TimeSync;
use tracing::warn;

use super::{ctl, NS};

const OWNER: &str = "time-sync";

/// Step the clock only when the offset exceeds this (128 ms, in ns).
const STEP_THRESHOLD_NS: i128 = 128_000_000;

/// Default NTP servers when config supplies none.
const DEFAULT_SERVERS: [&str; 2] = ["0.pool.ntp.org", "1.pool.ntp.org"];

pub struct TimeSyncController {
    sync: Arc<dyn TimeSync>,
    provider: Provider,
    sync_count: u64,
}

impl TimeSyncController {
    pub fn new(sync: Arc<dyn TimeSync>, provider: Provider) -> Self {
        Self {
            sync,
            provider,
            sync_count: 0,
        }
    }
}

fn status_obj(synced: bool, server: &str, offset_ns: i64, sync_count: u64) -> ResourceObject {
    ResourceObject::new(
        NS,
        "time",
        Resource::TimeStatus(TimeStatus {
            synced,
            server: server.to_string(),
            offset_ns,
            sync_count,
        }),
    )
}

#[async_trait]
impl Controller for TimeSyncController {
    fn name(&self) -> &str {
        OWNER
    }

    fn inputs(&self) -> Vec<Input> {
        Vec::new()
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output {
            typ: ResourceType::TimeStatus,
            kind: OutputKind::Exclusive,
        }]
    }

    fn resync_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(11 * 60))
    }

    async fn reconcile(&mut self, ctx: &ReconcileCtx) -> machined_runtime_core::Result<()> {
        let time_cfg = self.provider.time();
        if time_cfg.disabled {
            reconcile_owned(
                &ctx.state,
                OWNER,
                NS,
                ResourceType::TimeStatus,
                vec![status_obj(false, "", 0, self.sync_count)],
            )?;
            return Ok(());
        }

        let servers: Vec<String> = if time_cfg.servers.is_empty() {
            DEFAULT_SERVERS.iter().map(|s| s.to_string()).collect()
        } else {
            time_cfg.servers.clone()
        };

        for server in &servers {
            let addr = format!("{server}:123");
            match self.sync.query_offset(&addr).await {
                Ok(offset) => {
                    if offset.abs() > STEP_THRESHOLD_NS {
                        self.sync.step_clock(offset).map_err(ctl)?;
                    }
                    self.sync_count += 1;
                    reconcile_owned(
                        &ctx.state,
                        OWNER,
                        NS,
                        ResourceType::TimeStatus,
                        vec![status_obj(true, server, offset as i64, self.sync_count)],
                    )?;
                    return Ok(());
                }
                Err(e) => warn!(server = %server, error = %e, "ntp query failed; trying next"),
            }
        }

        // No server answered — transient; the timer retries. Report not-synced.
        reconcile_owned(
            &ctx.state,
            OWNER,
            NS,
            ResourceType::TimeStatus,
            vec![status_obj(false, "", 0, self.sync_count)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machined_config::{MachineConfig, MachineSection, TimeSection};
    use machined_resources::Key;
    use machined_runtime_core::{ReconcileCtx, State};
    use machined_time::FakeTimeSync;

    fn provider(servers: Vec<&str>, disabled: bool) -> Provider {
        Provider::new(MachineConfig {
            machine: MachineSection {
                hostname: None,
                sysctls: vec![],
                services: vec![],
                network: Default::default(),
                install: None,
                time: TimeSection {
                    servers: servers.into_iter().map(|s| s.to_string()).collect(),
                    disabled,
                },
            },
        })
    }

    fn time_status(state: &State) -> TimeStatus {
        match state
            .get(&Key::new(NS, ResourceType::TimeStatus, "time"))
            .unwrap()
            .spec
        {
            Resource::TimeStatus(t) => t,
            _ => panic!("wrong type"),
        }
    }

    #[tokio::test]
    async fn syncs_and_steps_past_threshold() {
        let fake = Arc::new(FakeTimeSync::new().with_offset("a:123", 500_000_000)); // 500ms
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = TimeSyncController::new(fake.clone(), provider(vec!["a"], false));
        c.reconcile(&ctx).await.unwrap();

        assert_eq!(fake.steps(), vec![500_000_000]); // stepped
        let st = time_status(&state);
        assert!(st.synced);
        assert_eq!(st.server, "a");
        assert_eq!(st.sync_count, 1);
    }

    #[tokio::test]
    async fn small_offset_is_not_stepped() {
        let fake = Arc::new(FakeTimeSync::new().with_offset("a:123", 1_000_000)); // 1ms < 128ms
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = TimeSyncController::new(fake.clone(), provider(vec!["a"], false));
        c.reconcile(&ctx).await.unwrap();
        assert!(fake.steps().is_empty());
        assert!(time_status(&state).synced);
    }

    #[tokio::test]
    async fn disabled_does_not_query() {
        let fake = Arc::new(FakeTimeSync::new().with_offset("a:123", 500_000_000));
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = TimeSyncController::new(fake.clone(), provider(vec!["a"], true));
        c.reconcile(&ctx).await.unwrap();
        assert!(fake.steps().is_empty());
        assert!(!time_status(&state).synced);
    }

    #[tokio::test]
    async fn all_unreachable_reports_unsynced() {
        let fake = Arc::new(FakeTimeSync::new()); // no offsets → all error
        let state = State::new();
        let ctx = ReconcileCtx {
            state: state.clone(),
        };
        let mut c = TimeSyncController::new(fake.clone(), provider(vec!["a", "b"], false));
        c.reconcile(&ctx).await.unwrap(); // Ok, not Err
        assert!(!time_status(&state).synced);
    }
}
```

- [ ] **Step 3: Wire the module + test + commit**

In `crates/controllers/src/lib.rs`, add `pub mod time;` (alongside `pub mod block;`/`pub mod network;`).

Run: `cargo test -p machined-controllers time` → the four controller tests pass.
Run: `cargo test -p machined-controllers` → all pass.
Run: `cargo clippy -p machined-controllers --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/controllers
git commit -m "feat(controllers): TimeSyncController (periodic SNTP sync)"
```

---

## Task 5: machined wiring + e2e

**Files:**
- Modify: `crates/machined/Cargo.toml` (add machined-time dep)
- Modify: `crates/machined/src/main.rs`
- Create: `crates/machined/tests/time.rs`

- [ ] **Step 1: Add the dep + register the controller**

In `crates/machined/Cargo.toml` `[dependencies]`, add `machined-time.workspace = true`.

In `crates/machined/src/main.rs`, add the import:

```rust
use machined_controllers::time::TimeSyncController;
```

Add a backend builder mirroring the others:

```rust
fn build_time_sync() -> Arc<dyn machined_time::TimeSync> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(machined_time::SntpTime::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(machined_time::FakeTimeSync::new())
    }
}
```

In `run_daemon`, after the block controllers and before the runtime spawn, register:

```rust
    runtime.register(Box::new(TimeSyncController::new(
        build_time_sync(),
        provider.clone(),
    )));
```

> `provider` is the config `Provider` already in scope (cloned for the other controllers); it still
> moves into `SequencerCtx` afterward.

- [ ] **Step 2: Build + smoke test**

Run: `cargo build --workspace` → PASS.
Run: `cargo run -p machined -- version` → `machined 0.1.0`.

- [ ] **Step 3: Write the e2e**

Create `crates/machined/tests/time.rs`:

```rust
//! End-to-end: the TimeSyncController on the real Runtime against a fake
//! TimeSync syncs and publishes TimeStatus. Root-free.

use std::sync::Arc;
use std::time::Duration;

use machined_config::{MachineConfig, MachineSection, Provider, TimeSection};
use machined_controllers::time::{TimeSyncController, NS};
use machined_resources::{Resource, ResourceType};
use machined_runtime_core::Runtime;
use machined_time::FakeTimeSync;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn syncs_time_and_publishes_status() {
    let sync = Arc::new(FakeTimeSync::new().with_offset("a:123", 300_000_000)); // 300ms
    let config = MachineConfig {
        machine: MachineSection {
            hostname: None,
            sysctls: vec![],
            services: vec![],
            network: Default::default(),
            install: None,
            time: TimeSection {
                servers: vec!["a".into()],
                disabled: false,
            },
        },
    };

    let mut runtime = Runtime::new();
    let state = runtime.state();
    runtime.register(Box::new(TimeSyncController::new(
        sync.clone(),
        Provider::new(config),
    )));

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move { runtime.run(token).await });

    let mut synced = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Ok(obj) = state.get(&machined_resources::Key::new(
            NS,
            ResourceType::TimeStatus,
            "time",
        )) {
            if let Resource::TimeStatus(t) = obj.spec {
                if t.synced {
                    synced = true;
                    break;
                }
            }
        }
    }
    assert!(synced, "time did not sync");
    assert_eq!(sync.steps(), vec![300_000_000]);

    shutdown.cancel();
    let _ = handle.await;
}
```

- [ ] **Step 4: Full gate + commit**

Run: `cargo test -p machined --test time` → PASS.
Run: `make pre-commit` → fmt + clippy -D warnings + full workspace test green (ignored tests stay ignored).

```bash
git add crates/machined Cargo.lock
git commit -m "feat(machined): register TimeSyncController + e2e time test"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** `Controller::resync_interval` periodic reconcile + tests (Task 1) ✓; `TimeStatus` + `time` config (Task 2) ✓; `time` crate (pure SNTP build/parse + `TimeSync` + `SntpTime` + fake) + loopback + gated clock test (Task 3) ✓; `TimeSyncController` (threshold step, disabled, all-unreachable) (Task 4) ✓; machined wiring + e2e (Task 5) ✓.
- **Deliberate M2c limits (per spec):** step-only (no slew); single-sample SNTP (no full NTP algorithm); no NTP serving/auth/leap-seconds; config read at construction. The `nix` clock + UDP are the only spike surface (isolated to `real.rs`); the trait/helpers/controller are deterministic and fully tested without privilege.
- **Periodic correctness:** `interval_at(now + d, d)` so the first tick is after `d` (no double-fire with the initial reconcile); the `tick(Option<&mut Interval>)` helper makes the timer arm inert for `None` controllers. The periodic test asserts ≥3 reconciles for a 20 ms interval and exactly 1 for `None`.
- **Type consistency:** `TimeSync`/`TimeOffset` (time) ↔ `TimeStatus` (resources) ↔ `TimeSection` (config); `reconcile_owned`/`Controller`/`Output(Exclusive)`/`resync_interval` (runtime-core). The controller appends `:123` to config hostnames; the loopback test uses a full `host:port`.
- **Config field follow-through:** adding `MachineSection.time` breaks the four explicit literals (E0063) — Task 2 Step 5 updates them with `time: Default::default()`.
- **Placeholder scan:** none; Task 3/5's `nix`/UDP code is real best-effort with an explicit spike protocol.
