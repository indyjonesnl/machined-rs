# machined-rs M5b — Reset Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M5a merged to `main`. Work on branch `spec/machined-rs-m5b-reset`.

**Goal:** `machinectl reset --yes` → graceful stop → (runtime already cancelled — ordering fix) → unmount → re-format STATE+EPHEMERAL in place → reboot-to-reprovision. EFI/partition table untouched.

**Architecture:** A `Reset` RPC enqueues `NodeAction::Reset` (existing channel). machined maps it to `FinalAction::Reset`; the shutdown path is reordered (cancel runtime + join API **before** the sequencer) for every final action; `perform_reset` reads `VolumeStatus` STATE/EPHEMERAL from the store and calls `BlockProvisioner::format` on each, then reboots.

**Tech Stack:** existing crates only.

---

## File Structure

```
crates/apiserver/proto/machine.proto   # MODIFY: + Reset RPC
crates/apiserver/src/service.rs        # MODIFY: NodeAction::Reset + handler
crates/apiserver/tests/grpc.rs         # MODIFY: extend the actions test
crates/machinectl/src/main.rs          # MODIFY: reset subcommand (--yes) + parse test
crates/machinectl/tests/e2e.rs         # MODIFY: refusal + delivery assertions
crates/machined/src/main.rs            # MODIFY: FinalAction::Reset, reorder, fs_type_of, perform_reset + tests
```

---

## Task 1: Reset RPC + machinectl

**Files:**
- Modify: `crates/apiserver/proto/machine.proto`
- Modify: `crates/apiserver/src/service.rs`
- Modify: `crates/apiserver/tests/grpc.rs`
- Modify: `crates/machinectl/src/main.rs`
- Modify: `crates/machinectl/tests/e2e.rs`
- Modify: `crates/machined/src/main.rs` (ONLY the new match arm so the workspace compiles; the real reset lands in Task 2)

- [ ] **Step 1: proto + NodeAction + handler**

`machine.proto` service gains:

```proto
  rpc Reset(Empty) returns (Empty);
```

`service.rs`: add `Reset` to `NodeAction`; add the handler (same shape as reboot/shutdown):

```rust
    async fn reset(&self, _req: Request<Empty>) -> Result<Response<Empty>, Status> {
        tracing::info!("reset requested via API");
        self.actions
            .send(NodeAction::Reset)
            .await
            .map_err(|_| Status::unavailable("daemon is shutting down"))?;
        Ok(Response::new(Empty {}))
    }
```

machined's `select!` match on the action gains (Task-1 stopgap — keeps the workspace compiling;
Task 2 replaces it with the real mapping):

```rust
            Some(NodeAction::Reset) => FinalAction::Reboot, // Task 2: FinalAction::Reset
```

Extend `reboot_and_shutdown_enqueue_actions` in `grpc.rs` (rename NOT needed) with:

```rust
    client.reset(Empty {}).await.unwrap();
    assert_eq!(rx.recv().await, Some(NodeAction::Reset));
```

(Bump the test channel capacity to 3 if needed.)

- [ ] **Step 2: machinectl reset --yes**

`Command` enum gains:

```rust
    /// Wipe STATE + EPHEMERAL and reboot to reprovision (DESTRUCTIVE).
    Reset {
        /// Confirm the destructive reset.
        #[arg(long)]
        yes: bool,
    },
```

Match arm:

```rust
        Command::Reset { yes } => {
            if !yes {
                eprintln!("reset wipes STATE and EPHEMERAL; pass --yes to confirm");
                std::process::exit(2);
            }
            client.reset(Empty {}).await?;
            println!("reset requested");
        }
```

Parse test:

```rust
    #[test]
    fn parses_reset_with_and_without_yes() {
        let r = Cli::try_parse_from(["machinectl", "reset", "--yes"]).unwrap();
        assert!(matches!(r.command, Command::Reset { yes: true }));
        let r2 = Cli::try_parse_from(["machinectl", "reset"]).unwrap();
        assert!(matches!(r2.command, Command::Reset { yes: false }));
    }
```

NOTE on `exit(2)` placement: the `connect()` happens in `main` before the match — a refused reset
must NOT require a live server. Restructure minimally: match on `Command::Reset { yes: false }`
**before** `connect()`:

```rust
    if let Command::Reset { yes: false } = cli.command {
        eprintln!("reset wipes STATE and EPHEMERAL; pass --yes to confirm");
        std::process::exit(2);
    }
    let mut client = connect(&cli.bundle, &cli.endpoint).await?;
```

(then the in-match arm only handles `yes: true`; adjust the match accordingly — e.g.
`Command::Reset { .. } => { client.reset(...).await?; println!("reset requested"); }`.)

- [ ] **Step 3: e2e — refusal + delivery**

In `crates/machinectl/tests/e2e.rs`, after the reboot block (the channel from M3c has capacity 1 —
the reboot was consumed; reuse it):

```rust
    // `reset` without --yes refuses CLIENT-SIDE: non-zero exit, nothing sent.
    let refused = tokio::process::Command::new(bin)
        .args([
            "--bundle",
            bundle.to_str().unwrap(),
            "--endpoint",
            &endpoint,
            "reset",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(refused.status.code(), Some(2), "must refuse without --yes");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), action_rx.recv())
            .await
            .is_err(),
        "no action may reach the server on refusal"
    );

    // `reset --yes` delivers.
    let out4 = tokio::process::Command::new(bin)
        .args([
            "--bundle",
            bundle.to_str().unwrap(),
            "--endpoint",
            &endpoint,
            "reset",
            "--yes",
        ])
        .output()
        .await
        .unwrap();
    assert!(out4.status.success(), "reset --yes failed: {:?}", out4);
    assert_eq!(
        action_rx.recv().await,
        Some(machined_apiserver::NodeAction::Reset)
    );
```

- [ ] **Step 4: gates + commit**

`cargo test -p machined-apiserver -p machinectl` green (extended actions test + parse + e2e);
`cargo build --workspace`; clippy/fmt clean.

```bash
git add crates/apiserver crates/machinectl crates/machined
git commit -m "feat(apiserver,machinectl): Reset RPC + machinectl reset --yes"
```

---

## Task 2: machined — FinalAction::Reset + ordering fix + perform_reset

**Files:**
- Modify: `crates/machined/src/main.rs`

- [ ] **Step 1: FinalAction::Reset + real mapping**

Add `Reset` to `FinalAction`; replace the Task-1 stopgap arm:

```rust
            Some(NodeAction::Reset) => FinalAction::Reset,
```

- [ ] **Step 2: THE ORDERING FIX — cancel the runtime before the sequencer**

Reorder the shutdown path in `run_daemon`. Today:
`shutdown_sequence → shutdown.cancel() → rt join → API join → final_action`.
New (controllers must not act during unmount/format):

```rust
    info!("shutting down");

    // Stop the controller runtime FIRST: no controller may act (e.g. re-mount)
    // while services stop, volumes unmount, or a reset formats partitions.
    shutdown.cancel();
    let _ = rt_handle.await;
    if let Some(mut h) = api_handle {
        if tokio::time::timeout(std::time::Duration::from_secs(5), &mut h)
            .await
            .is_err()
        {
            warn!("api server did not shut down in time; aborting");
            h.abort();
            let _ = h.await;
        }
    }

    // Graceful stop + disk teardown.
    if let Err(e) = shutdown_sequence().run(&ctx).await {
        error!("shutdown sequence error: {e}");
    }
    info!("machined stopped");
```

> NOTE: the API server now stops BEFORE the stop sequence (it shares the runtime token). The M5a
> nicety of watching stop progress via machinectl is traded for format safety; acceptable and
> documented (the spec's ordering note). If a separate API token is ever wanted, that's M6 polish.

- [ ] **Step 3: fs_type_of + perform_reset + tests**

Add near the other helpers:

```rust
/// Map a VolumeStatus fs string to a mkfs type. Unknown → None (skip).
fn fs_type_of(fs: &str) -> Option<machined_block::FsType> {
    match fs {
        "ext4" => Some(machined_block::FsType::Ext4),
        "vfat" => Some(machined_block::FsType::Vfat),
        "xfs" => Some(machined_block::FsType::Xfs),
        "swap" => Some(machined_block::FsType::Swap),
        _ => None,
    }
}

/// Reset: re-format STATE + EPHEMERAL in place (labels preserved) so the next
/// boot reprovisions fresh volumes. Best-effort — failures log and the reset
/// still proceeds to reboot.
async fn perform_reset(
    state: &machined_runtime_core::State,
    prov: &dyn machined_block::BlockProvisioner,
) {
    use machined_resources::{Key, Resource, ResourceType};
    for label in ["STATE", "EPHEMERAL"] {
        let key = Key::new("block", ResourceType::VolumeStatus, label);
        let vol = match state.get(&key).map(|o| o.spec) {
            Ok(Resource::VolumeStatus(v)) => v,
            _ => {
                warn!("reset: no VolumeStatus for {label}; skipping");
                continue;
            }
        };
        let Some(fs) = fs_type_of(&vol.fs) else {
            warn!("reset: unknown fs '{}' for {label}; skipping", vol.fs);
            continue;
        };
        info!("reset: formatting {} ({}, {label})", vol.device, vol.fs);
        if let Err(e) = prov.format(&vol.device, fs, label).await {
            error!("reset: format {} failed: {e}", vol.device);
        }
    }
}
```

In the final-action match add:

```rust
        FinalAction::Reset => {
            info!("resetting: wiping STATE + EPHEMERAL, then rebooting");
            perform_reset(&state_for_reset, block_for_reset.as_ref()).await;
            if let Err(e) = platform.reboot() {
                error!("reboot failed: {e}");
            }
        }
```

Wiring for the two captured values: `state` moves into `SequencerCtx` — take ONE more clone before
that (`let state_for_reset = state.clone();` next to the API clone), and keep a
`let block_for_reset = block.clone();` next to where `block` is built (before the controllers
consume it).

Add tests to machined's `#[cfg(test)] mod tests` in main.rs:

```rust
    #[test]
    fn fs_type_maps() {
        assert!(matches!(fs_type_of("ext4"), Some(machined_block::FsType::Ext4)));
        assert!(matches!(fs_type_of("vfat"), Some(machined_block::FsType::Vfat)));
        assert!(fs_type_of("ntfs").is_none());
    }

    #[tokio::test]
    async fn reset_formats_exactly_state_and_ephemeral() {
        use machined_resources::{Resource, ResourceObject, VolumePhase, VolumeStatus};

        let state = State::new();
        for (label, dev, fs) in [
            ("EFI", "/dev/vda1", "vfat"),
            ("STATE", "/dev/vda2", "ext4"),
            ("EPHEMERAL", "/dev/vda3", "ext4"),
        ] {
            let _ = state.create(ResourceObject::new(
                "block",
                label,
                Resource::VolumeStatus(VolumeStatus {
                    name: label.into(),
                    device: dev.into(),
                    fs: fs.into(),
                    label: label.into(),
                    phase: VolumePhase::Provisioned,
                }),
            ));
        }
        let fake = machined_block::FakeBlockBackend::new();
        perform_reset(&state, &fake).await;

        let formats = fake.formats(); // adapt to the fake's real recording accessor
        assert_eq!(formats.len(), 2, "exactly STATE + EPHEMERAL");
        assert!(formats.iter().any(|f| f.0 == "/dev/vda2"));
        assert!(formats.iter().any(|f| f.0 == "/dev/vda3"));
        assert!(
            !formats.iter().any(|f| f.0 == "/dev/vda1"),
            "EFI must NEVER be formatted by reset"
        );
        // No wipes / re-partitioning.
        assert!(fake.wipes().is_empty()); // adapt accessor name
    }

    #[tokio::test]
    async fn reset_without_volumes_degrades_to_noop() {
        let state = State::new();
        let fake = machined_block::FakeBlockBackend::new();
        perform_reset(&state, &fake).await;
        assert!(fake.formats().is_empty());
    }
```

> **Adapt note:** check `FakeBlockBackend`'s real recording API (M2b gave it recorded
> wipes/partitions/formats — find the accessor/field names in `crates/block/src/fake.rs` or
> equivalent; if formats are recorded as a struct, match fields accordingly; if there is NO
> format recording, add a minimal `pub formats: Mutex<Vec<(String, FsType, String)>>` recording to
> the fake + accessor — that's an in-crate test-support addition, report it).

- [ ] **Step 4: full gates + commit**

`cargo test --workspace` green (the reorder must keep ALL M5a shutdown/boot/payload tests green —
if any fail, STOP and report which); `cargo run -p machined -- version` OK; `make pre-commit` green.

```bash
git add crates/machined crates/block
git commit -m "feat(machined): Reset final action (format STATE+EPHEMERAL, reboot) + runtime-cancel-before-sequence"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** Reset RPC + NodeAction + handler + actions test (T1) ✓; machinectl `reset --yes` (refusal BEFORE connect, exit 2) + parse + e2e refusal/delivery (T1) ✓; `FinalAction::Reset` + reorder (cancel runtime → joins → sequencer → final action) + `fs_type_of` + `perform_reset` (exactly-two-formats pinned, EFI-never, degrade-to-noop) (T2) ✓.
- **Destructive-op discipline:** formats only store-published STATE/EPHEMERAL devices; no wipe/create_partitions calls anywhere in the reset path (pinned); EFI assertion explicit.
- **Ordering trade documented:** the API server stops before the stop sequence post-reorder (shares the runtime token) — spec'd and noted inline.
- **Type consistency:** `NodeAction::Reset` (apiserver) → `FinalAction::Reset` (machined) → `perform_reset(&State, &dyn BlockProvisioner)`; `fs_type_of` lives in machined (YAGNI).
- **Placeholder scan:** the only adapt-point is the fake's format-recording accessor (flagged).
