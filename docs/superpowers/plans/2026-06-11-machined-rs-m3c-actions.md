# machined-rs M3c — Action RPCs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M3a merged to `main`. Work on branch `spec/machined-rs-m3c-actions`.

**Goal:** `Reboot` + `Shutdown` RPCs that hand a `NodeAction` to the daemon's main loop, which runs the existing graceful `shutdown_sequence` and then `platform.reboot()`/`poweroff()`, plus `machinectl reboot`/`shutdown` subcommands.

**Architecture:** The `apiserver` `Machine` service gains an `mpsc::Sender<NodeAction>`; the `Reboot`/`Shutdown` handlers enqueue an action and return `Ok` (accepted). `machined::run_daemon` selects its post-boot wait on the OS signal **or** an action; the resulting `FinalAction` drives `reboot(2)`/`poweroff(2)` after the shutdown sequence — never a direct platform call in the handler.

**Tech Stack:** `tokio::sync::mpsc`, the M3a `apiserver`/`machinectl`/`pki` crates, `tonic`.

---

## File Structure

```
crates/apiserver/proto/machine.proto   # MODIFY: + Reboot, Shutdown RPCs
crates/apiserver/src/service.rs        # MODIFY: NodeAction, Machine.actions, handlers
crates/apiserver/src/lib.rs            # MODIFY: serve() gains actions; re-export NodeAction
crates/apiserver/tests/grpc.rs         # MODIFY: update 3 Machine::new sites + actions test
crates/machinectl/src/main.rs          # MODIFY: reboot/shutdown subcommands + parse tests
crates/machinectl/tests/e2e.rs         # MODIFY: real action_rx + reboot e2e assertion
crates/machined/src/main.rs            # MODIFY: action channel + select FinalAction + platform call
```

---

## Task 1: `apiserver` — NodeAction + Reboot/Shutdown handlers

**Files:**
- Modify: `crates/apiserver/proto/machine.proto`
- Modify: `crates/apiserver/src/service.rs`
- Modify: `crates/apiserver/src/lib.rs`
- Modify: `crates/apiserver/tests/grpc.rs`
- Modify: `crates/machinectl/tests/e2e.rs` (call-site fix to keep the workspace green)
- Modify: `crates/machined/src/main.rs` (call-site fix; real wiring is Task 3)

- [ ] **Step 1: Add the proto RPCs**

In `crates/apiserver/proto/machine.proto`, add two RPCs to the service:

```proto
service MachineService {
  rpc Version(Empty) returns (VersionResponse);
  rpc ListResources(ListResourcesRequest) returns (ListResourcesResponse);
  rpc Reboot(Empty) returns (Empty);
  rpc Shutdown(Empty) returns (Empty);
}
```

(No new messages — both reuse `Empty`.)

- [ ] **Step 2: Add NodeAction + the channel + handlers**

In `crates/apiserver/src/service.rs`, add the import + enum at the top (after the existing `use` lines):

```rust
use tokio::sync::mpsc;

/// A node lifecycle action requested via the API, handed to the daemon main loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAction {
    Reboot,
    Shutdown,
}
```

Change the `Machine` struct + `new` to carry the sender:

```rust
pub struct Machine {
    state: State,
    version: String,
    actions: mpsc::Sender<NodeAction>,
}

impl Machine {
    pub fn new(state: State, version: impl Into<String>, actions: mpsc::Sender<NodeAction>) -> Self {
        Self {
            state,
            version: version.into(),
            actions,
        }
    }
}
```

Add the two handler methods inside `impl MachineService for Machine` (after `list_resources`):

```rust
    async fn reboot(&self, _req: Request<Empty>) -> Result<Response<Empty>, Status> {
        tracing::info!("reboot requested via API");
        self.actions
            .send(NodeAction::Reboot)
            .await
            .map_err(|_| Status::unavailable("daemon is shutting down"))?;
        Ok(Response::new(Empty {}))
    }

    async fn shutdown(&self, _req: Request<Empty>) -> Result<Response<Empty>, Status> {
        tracing::info!("shutdown requested via API");
        self.actions
            .send(NodeAction::Shutdown)
            .await
            .map_err(|_| Status::unavailable("daemon is shutting down"))?;
        Ok(Response::new(Empty {}))
    }
```

- [ ] **Step 3: Thread the sender through `serve` + re-export NodeAction**

In `crates/apiserver/src/lib.rs`, update `serve` and the re-export:

```rust
pub use service::{Machine, NodeAction};
```

```rust
pub async fn serve(
    addr: SocketAddr,
    state: State,
    version: impl Into<String>,
    pki: &NodePki,
    actions: tokio::sync::mpsc::Sender<NodeAction>,
) -> Result<(), tonic::transport::Error> {
    let svc =
        pb::machine_service_server::MachineServiceServer::new(Machine::new(state, version, actions));
    let tls = server_tls(pki);
    Server::builder()
        .tls_config(tls)?
        .add_service(svc)
        .serve(addr)
        .await
}
```

- [ ] **Step 4: Fix the read-only call sites (keep the workspace compiling)**

Each existing `Machine::new(state, "x.y.z")` now needs a sender. In `crates/apiserver/tests/grpc.rs`,
update **all three** `Machine::new(state, "9.9.9")` calls to add a throwaway sender. Replace each:

```rust
        Machine::new(state, "9.9.9"),
```
with:
```rust
        Machine::new(state, "9.9.9", tokio::sync::mpsc::channel(1).0),
```
(The dropped receiver is fine — these tests never call `reboot`/`shutdown`.)

In `crates/machinectl/tests/e2e.rs`, update the one `Machine::new(state, "1.2.3")` the same way:

```rust
            machined_apiserver::Machine::new(state, "1.2.3", tokio::sync::mpsc::channel(1).0),
```

In `crates/machined/src/main.rs`, the `serve(api_addr, api_state, env!("CARGO_PKG_VERSION"), &pki)`
call now needs a sender. For Task 1, pass a throwaway (Task 3 replaces this with the real channel).
Immediately before the `tokio::spawn(async move { ... serve(...) ... })`, add:

```rust
            let (api_action_tx, _api_action_rx) = tokio::sync::mpsc::channel(1);
```

and update the `serve` call inside the spawn to:

```rust
                if let Err(e) = machined_apiserver::serve(
                    api_addr,
                    api_state,
                    env!("CARGO_PKG_VERSION"),
                    &pki,
                    api_action_tx,
                )
                .await
```

> `_api_action_rx` is intentionally unused for now (leading underscore avoids the warning); Task 3
> turns it into the real receiver the main loop selects on.

- [ ] **Step 5: Add the actions integration test**

Append to `crates/apiserver/tests/grpc.rs`:

```rust
#[tokio::test]
async fn reboot_and_shutdown_enqueue_actions() {
    use machined_apiserver::pb::machine_service_client::MachineServiceClient;
    use machined_apiserver::pb::Empty;
    use machined_apiserver::{Machine, NodeAction};

    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let svc = machined_apiserver::pb::machine_service_server::MachineServiceServer::new(
        Machine::new(State::new(), "9.9.9", tx),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client = MachineServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    client.reboot(Empty {}).await.unwrap();
    assert_eq!(rx.recv().await, Some(NodeAction::Reboot));
    client.shutdown(Empty {}).await.unwrap();
    assert_eq!(rx.recv().await, Some(NodeAction::Shutdown));
}
```

- [ ] **Step 6: Build + test + commit**

Run: `cargo build --workspace` → PASS (every `Machine::new`/`serve` site updated — a missed one is a compile error; grep `Machine::new` if it fails).
Run: `cargo test -p machined-apiserver` → mapping unit + plaintext/mTLS integration + `reboot_and_shutdown_enqueue_actions` pass.
Run: `cargo clippy --all-targets --all-features -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean (run `cargo fmt --all` first).

```bash
git add crates/apiserver crates/machinectl crates/machined
git commit -m "feat(apiserver): Reboot/Shutdown RPCs enqueueing NodeAction"
```

---

## Task 2: `machinectl` — reboot/shutdown subcommands

**Files:**
- Modify: `crates/machinectl/src/main.rs`
- Modify: `crates/machinectl/tests/e2e.rs`

- [ ] **Step 1: Add the subcommands**

In `crates/machinectl/src/main.rs`, add two variants to the `Command` enum (after `Get`):

```rust
    /// Reboot the node.
    Reboot,
    /// Power the node off.
    Shutdown,
```

Add the match arms in `main` (after the `Command::Get { .. }` arm):

```rust
        Command::Reboot => {
            client.reboot(Empty {}).await?;
            println!("reboot requested");
        }
        Command::Shutdown => {
            client.shutdown(Empty {}).await?;
            println!("shutdown requested");
        }
```

(The generated `MachineServiceClient` already has `reboot`/`shutdown` methods from the proto; `Empty`
is already imported.)

- [ ] **Step 2: Add parse unit tests**

In the `tests` module of `main.rs`, add:

```rust
    #[test]
    fn parses_reboot_and_shutdown() {
        let r = Cli::try_parse_from(["machinectl", "reboot"]).unwrap();
        assert!(matches!(r.command, Command::Reboot));
        let s = Cli::try_parse_from(["machinectl", "shutdown"]).unwrap();
        assert!(matches!(s.command, Command::Shutdown));
    }
```

- [ ] **Step 3: Extend the e2e to assert the reboot reaches the server**

In `crates/machinectl/tests/e2e.rs`, change the server setup to hold the real action receiver and add
a reboot assertion. Replace the `Machine::new(state, "1.2.3", tokio::sync::mpsc::channel(1).0)` line
(from Task 1) so the test owns the receiver: just before the server `tokio::spawn`, add

```rust
    let (action_tx, mut action_rx) = tokio::sync::mpsc::channel(1);
```

and change the `Machine::new(...)` inside the spawn to:

```rust
            machined_apiserver::Machine::new(state, "1.2.3", action_tx),
```

Then, after the existing `get ServiceStatus` assertion block (and before `remove_dir_all`), add:

```rust
    // `reboot` reaches the handler → the action is enqueued.
    let out3 = tokio::process::Command::new(bin)
        .args([
            "--bundle",
            bundle.to_str().unwrap(),
            "--endpoint",
            &endpoint,
            "reboot",
        ])
        .output()
        .await
        .unwrap();
    assert!(out3.status.success(), "reboot failed: {:?}", out3);
    assert_eq!(
        action_rx.recv().await,
        Some(machined_apiserver::NodeAction::Reboot)
    );
```

- [ ] **Step 4: Test + clippy + commit**

Run: `cargo test -p machinectl` → unit (3) + `machinectl_queries_a_real_server` (now also asserting reboot) pass.
Run: `cargo clippy -p machinectl --all-targets -- -D warnings` → clean.
Run: `cargo fmt --all -- --check` → clean.

```bash
git add crates/machinectl
git commit -m "feat(machinectl): reboot + shutdown subcommands"
```

---

## Task 3: `machined` — graceful action path

**Files:**
- Modify: `crates/machined/src/main.rs`

- [ ] **Step 1: Add the FinalAction enum + the real action channel**

In `crates/machined/src/main.rs`, add near the top-level helpers (e.g. above `run_daemon`):

```rust
/// What the daemon does after the graceful shutdown sequence.
enum FinalAction {
    Stop,
    Reboot,
    Poweroff,
}
```

Add the import for `NodeAction`:

```rust
use machined_apiserver::NodeAction;
```

Replace the Task-1 throwaway channel. Find:

```rust
            let (api_action_tx, _api_action_rx) = tokio::sync::mpsc::channel(1);
```

There is a scoping problem: the receiver must live in `run_daemon`'s body (used by the post-boot
`select!`), not inside the `match NodePki { Ok(pki) => { ... } }` block. So **move the channel creation
out** to just before the PKI/management-API block, at `run_daemon` scope:

```rust
    // Action channel: API handlers enqueue; the main loop below consumes one.
    let (api_action_tx, mut api_action_rx) = tokio::sync::mpsc::channel::<NodeAction>(1);
```

and inside the management-API `Ok(pki)` arm, delete the old `let (api_action_tx, _api_action_rx) = ...`
line and pass the now-outer `api_action_tx` (clone it so the outer binding/move rules are satisfied):
the `serve(...)` call uses `api_action_tx.clone()`.

- [ ] **Step 2: Select the post-boot wait on signal OR action**

Replace:

```rust
    // Wait for a termination signal.
    pid1::wait_for_termination().await;
    info!("shutting down");
```

with:

```rust
    // Wait for an OS termination signal OR an API-requested action.
    let final_action = tokio::select! {
        _ = pid1::wait_for_termination() => FinalAction::Stop,
        a = api_action_rx.recv() => match a {
            Some(NodeAction::Reboot) => FinalAction::Reboot,
            Some(NodeAction::Shutdown) => FinalAction::Poweroff,
            None => FinalAction::Stop,
        },
    };
    info!("shutting down");
```

- [ ] **Step 3: Perform the final action after the shutdown sequence**

After the existing runtime stop (`shutdown.cancel(); let _ = rt_handle.await;` and the
`info!("machined stopped");`), but before `Ok(())`, add:

```rust
    match final_action {
        FinalAction::Stop => {}
        FinalAction::Reboot => {
            info!("rebooting");
            if let Err(e) = platform.reboot() {
                error!("reboot failed: {e}");
            }
        }
        FinalAction::Poweroff => {
            info!("powering off");
            if let Err(e) = platform.poweroff() {
                error!("poweroff failed: {e}");
            }
        }
    }
```

> `platform` is the `Arc<dyn Platform>` already in scope (used for `SequencerCtx`/emergency). On a real
> node `reboot()/poweroff()` does not return; the error arm is only reached on the fake/unprivileged
> path. `info!("machined stopped")` stays before this match.

- [ ] **Step 4: Build + smoke + full gate + commit**

Run: `cargo build --workspace` → PASS.
Run: `cargo run -p machined -- version` → `machined 0.1.0` (no daemon/select on the version path).
Run: `make pre-commit` → fmt + clippy -D warnings + full workspace test green.

```bash
git add crates/machined
git commit -m "feat(machined): API-driven reboot/shutdown via graceful action path"
```

---

## Self-Review Notes (for the implementer)

- **Spec coverage:** `Reboot`/`Shutdown` proto + `NodeAction` + handlers enqueueing + `serve`/`Machine::new` sender (Task 1) ✓; `machinectl reboot`/`shutdown` subcommands + parse tests + reboot e2e (Task 2) ✓; `machined` action channel + `select!` + `FinalAction` + `platform.reboot/poweroff` after the graceful sequence (Task 3) ✓.
- **Signature-change blast radius handled in Task 1:** all 5 `Machine::new` sites (3 grpc.rs + 1 e2e.rs + 1 inside `serve`) and the 1 `serve` call site updated, so the workspace stays green after each task. Task 2 upgrades the e2e's throwaway sender to a real held receiver; Task 3 upgrades machined's throwaway to the real select-driven receiver.
- **Graceful, not mid-RPC kill:** handlers only `send().await` a `NodeAction` and return `Ok`; the daemon acts only after `recv()` + the full `shutdown_sequence` + runtime stop. No `platform.reboot()` in any handler.
- **Type consistency:** `NodeAction` (apiserver, `PartialEq`/`Debug` for the test asserts) flows handler → `mpsc::Sender<NodeAction>` → `api_action_rx` → `FinalAction` (machined-local) → `platform.reboot()/poweroff()`. `serve(addr, state, version, pki, actions)` is the single new signature.
- **Deliberate M3c limits:** Reboot+Shutdown only (Reset/upgrade = M5); mTLS-only authz (no RBAC); no confirmation/`--wait`. Channel capacity 1 (one action ends the node).
- **Placeholder scan:** none; every step ships complete code + exact commands.
```
