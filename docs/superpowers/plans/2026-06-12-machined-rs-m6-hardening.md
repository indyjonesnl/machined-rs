# machined-rs M6 — Hardening Sweep Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Prerequisite:** M5 merged to `main`. Work on branch `spec/machined-rs-m6-hardening`.

**Goal:** Close the review carry-forwards: group kill + `Stopped` state (A), reset fs-fallback + `FsType` dedup + lazy-unmount escalation (B), park-on-failed-final-syscall + PKI dir hygiene (C), `make root-tests` + harness ordering mirror (D).

**Architecture:** Four independent task groups; each is small, test-pinned, and confined to the crates it names. No new dependencies.

**Tech Stack:** existing crates; `Command::process_group(0)` (tokio unix), `nix killpg`, `umount2(MNT_DETACH)`.

---

## Task A: process-group kill + Stopped state

**Files:** `crates/resources/src/resource.rs`, `crates/supervisor/src/{process,service,manager,readiness}.rs`

- [ ] **Step 1:** resources: add `ServiceState::Stopped` (after `Waiting`), doc `/// Drained by a stop request.` Build the workspace; fix any E0004 (none expected).

- [ ] **Step 2:** process.rs: in `run()`, the `Command` gains (before `.kill_on_drop(true)`):

```rust
        #[cfg(unix)]
        let child = {
            let mut cmd = Command::new(program);
            cmd.args(args)
                // Own process group so stop can signal the whole tree
                // (grandchildren of forking payloads must die too).
                .process_group(0)
                .kill_on_drop(true);
            cmd.spawn()
        }
        .map_err(...)?;
```

(Restructure the existing builder minimally; on non-unix keep the old form behind `#[cfg(not(unix))]`. If the resolved tokio lacks `process_group`, use `std::os::unix::process::CommandExt::process_group` via `.as_std_mut()` or fall back to `unsafe { cmd.pre_exec(|| { nix::unistd::setsid()?; Ok(()) }) }` — report which.)

- [ ] **Step 3:** manager.rs `stop_all`: replace `kill(Pid, SIGTERM)` with `killpg`:

```rust
                    use nix::sys::signal::{killpg, Signal};
                    use nix::unistd::Pid;
                    let pgid = Pid::from_raw(pid as i32); // leader pid == pgid
                    match killpg(pgid, Signal::SIGTERM) {
                        Ok(()) => info!(service = %h.id, "sent SIGTERM to group"),
                        Err(nix::errno::Errno::ESRCH) => {}
                        Err(e) => warn!(service = %h.id, "SIGTERM failed: {e}"),
                    }
```

and in the grace-expiry arm, BEFORE `h.join.abort()`:

```rust
                    #[cfg(unix)]
                    if let Some(pid) = *h.pid.lock().unwrap() {
                        use nix::sys::signal::{killpg, Signal};
                        use nix::unistd::Pid;
                        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
                    }
```

- [ ] **Step 4:** service.rs `run_supervised`: stop-honest final status. After `let outcome = run_service(...)`:

```rust
        if stop.load(Ordering::SeqCst) {
            publish_status(state, &id, ServiceState::Stopped, true, "drained");
            return;
        }
        if !should_restart(policy, outcome) {
            return;
        }
```

and the two stop-while-gated publishes change from `Finished, true, "stopped"` to
`Stopped, true, "stopped"`.

- [ ] **Step 5:** tests. manager.rs `graceful_stop_drains_on_sigterm` now expects `Stopped` (was
`Finished`). readiness truth table gains `put(... Stopped ...)` + `assert!(!r.is_ready(..))`.
New group-kill test in manager.rs:

```rust
    #[tokio::test]
    async fn stop_kills_the_whole_process_group() {
        let state = State::new();
        let mut mgr = ServiceManager::new(state.clone());
        // sh forks a grandchild sleep; on TERM sh exits 0 but WITHOUT group
        // signaling the grandchild would survive.
        mgr.start_all(
            &[svc_full(
                "forker",
                &["sh", "-c", "sleep 30 & trap 'exit 0' TERM; wait"],
                5,
            )],
            Arc::new(crate::readiness::DefaultReadiness),
        )
        .unwrap();
        wait_running(&state, "forker").await;
        let pgid = mgr.handles[0].pid.lock().unwrap().expect("pid");

        mgr.stop_all().await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The WHOLE group is gone (grandchild included): signal 0 probe → ESRCH.
        use nix::sys::signal::killpg;
        use nix::unistd::Pid;
        assert_eq!(
            killpg(Pid::from_raw(pgid as i32), None),
            Err(nix::errno::Errno::ESRCH),
            "process group must be fully dead"
        );
    }
```

(`killpg(pgid, None)` is the existence probe. `mgr.handles` is private but the test lives in the
same module.)

- [ ] **Step 6:** gates + commit: supervisor suite green (no hangs), workspace green, clippy/fmt.

```bash
git add crates/resources crates/supervisor
git commit -m "feat(supervisor): process-group termination + honest Stopped state"
```

---

## Task B: reset fs fallback + FsType dedup + lazy-unmount escalation

**Files:** `crates/block/src/lib.rs`, `crates/machined/src/main.rs`, `crates/platform/src/{lib,linux,fake}.rs`, `crates/sequencer/src/shutdown.rs`

- [ ] **Step 1:** block: add to `FsType`:

```rust
impl FsType {
    /// Parse the canonical lowercase name ("ext4", "vfat", "xfs", "swap").
    pub fn from_str_name(s: &str) -> Option<Self> {
        Some(match s {
            "ext4" => FsType::Ext4,
            "vfat" => FsType::Vfat,
            "xfs" => FsType::Xfs,
            "swap" => FsType::Swap,
            _ => return None,
        })
    }
}
```

+ a table test. machined: delete `fs_type_of`, use `FsType::from_str_name`; `fs_type_maps` test
moves to block (machined keeps no mapping test).

- [ ] **Step 2:** machined `perform_reset`: NS dedup + fallback:

```rust
    use machined_controllers::block::NS as BLOCK_NS;
    // per-label fallback when the recorded fs is empty/unknown (corrupt fs —
    // exactly the volume reset most needs to wipe): the fixed layout's type.
    fn fallback_fs(label: &str) -> Option<machined_block::FsType> {
        match label {
            "STATE" | "EPHEMERAL" => Some(machined_block::FsType::Ext4),
            _ => None,
        }
    }
```

`let Some(fs) = FsType::from_str_name(&vol.fs).or_else(|| fallback_fs(label)) else { warn + continue }`.
Key uses `BLOCK_NS`. Extend the reset tests: a STATE volume with `fs: ""` still gets formatted ext4.

- [ ] **Step 3:** platform: trait + impls `unmount_lazy(&self, target) -> Result<()>` —
linux `umount2(target, MntFlags::MNT_DETACH)`; fake records `disk_ops.push(format!("unmount_lazy:{t}"))`
+ removes the mount + pushes into `unmounts`. Fake failure injection for PLAIN unmount:

```rust
    /// Targets whose plain unmount fails (busy simulation). Lazy always works.
    pub fail_unmount_targets: Mutex<Vec<String>>,
```

`unmount()` first checks membership → `Err(PlatformError::Mount { target, message: "busy (fake)" })`
(do NOT remove the mount or record). Unit test: fail-target plain unmount errs, lazy succeeds + flips
`is_mounted`.

- [ ] **Step 4:** sequencer `SyncAndUnmount`: escalate:

```rust
                Ok(true) => {
                    if let Err(e) = ctx.platform.unmount(target) {
                        tracing::warn!("unmount {target}: {e}; retrying lazy (MNT_DETACH)");
                        if let Err(e2) = ctx.platform.unmount_lazy(target) {
                            tracing::warn!("lazy unmount {target}: {e2}");
                        }
                    }
                }
```

Extend the shutdown test: mark `/var` as fail-plain → `disk_ops == ["sync", "unmount_lazy:/var",
"unmount:/system/state"]`.

- [ ] **Step 5:** gates + commit.

```bash
git add crates/block crates/machined crates/platform crates/sequencer
git commit -m "feat(block,platform): reset fs fallback + FsType::from_str_name + lazy-unmount escalation"
```

---

## Task C: park-on-failed-final + PKI dir hygiene

**Files:** `crates/machined/src/main.rs`, `crates/pki/src/lib.rs`

- [ ] **Step 1:** machined: the park helper + use in all three final arms:

```rust
/// A final syscall (reboot/poweroff) failed: PID1 must never exit. Enter the
/// emergency state and park forever.
async fn park_after_failed_final(platform: &Arc<dyn Platform>, err: &dyn std::fmt::Display) {
    emergency::enter_emergency(platform, err, false);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}
```

Each `FinalAction::{Reboot,Poweroff,Reset}` arm: `if let Err(e) = platform.reboot() {
error!(...); park_after_failed_final(&platform, &e).await; }` (same for poweroff). Test (in
machined's mod tests):

```rust
    #[tokio::test]
    async fn failed_final_parks_forever() {
        let platform: Arc<dyn Platform> = Arc::new(machined_platform::FakePlatform::new());
        let parked = tokio::spawn(async move {
            park_after_failed_final(&platform, &"reboot failed (test)").await;
        });
        // It must NOT return.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), parked)
                .await
                .is_err(),
            "park must never complete"
        );
    }
```

(Confirm `emergency::enter_emergency` is non-blocking with `reboot_on_failure: false` — it is (M1);
if its module is private to main.rs adjust the path.)

- [ ] **Step 2:** pki: `PkiError::Partial(Vec<String>)` (`#[error("partial PKI dir; missing: {0:?}")]`).
`load_or_generate`: compute `missing: Vec<&str>` of the 4 names; all present → load; all missing →
generate; else → `Err(PkiError::Partial(...))`. Dir: after `create_dir_all`, `set_permissions(dir,
0o700)` (unix). Tests:

```rust
    #[test]
    fn partial_pki_dir_errors_not_rekeys() {
        let dir = std::env::temp_dir().join(format!("mnd-pki-part-{}", std::process::id()));
        NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        std::fs::remove_file(dir.join("server.key")).unwrap();
        let err = NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap_err();
        assert!(err.to_string().contains("server.key"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn pki_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mnd-pki-700-{}", std::process::id()));
        NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 3:** gates + commit.

```bash
git add crates/machined crates/pki
git commit -m "feat(machined,pki): park on failed final syscall + PKI dir 0700/partial-dir error"
```

---

## Task D: root-tests target + harness ordering mirror

**Files:** `Makefile`, `README.md`, `crates/machined/tests/boot_harness.rs`

- [ ] **Step 1:** Makefile:

```makefile
# Privileged test tier: loop devices, netns, clock_settime, real containerd.
# Run on a Linux host with sudo; containerd test additionally needs a running
# containerd at /run/containerd/containerd.sock.
root-tests:
	sudo -E cargo test -p machined-platform -p machined-block -p machined-netlink -p machined-time -p machined-cri -- --ignored
```

(add `root-tests` to `.PHONY`). README: a short "Privileged tests" section pointing at it.

- [ ] **Step 2:** boot_harness mirrors production ordering — move `shutdown.cancel(); rt_handle.await`
BEFORE `shutdown_sequence().run(&ctx)` (with a comment: mirrors run_daemon's
runtime-cancel-before-sequence). Suite must stay green.

- [ ] **Step 3:** full gates + commit. `make pre-commit` green; best-effort: run `make root-tests`
if sudo is available non-interactively, else note skipped.

```bash
git add Makefile README.md crates/machined
git commit -m "chore: make root-tests target + harness mirrors shutdown ordering"
```

---

## Self-Review Notes

- **Spec coverage:** A (group(0) spawn, killpg TERM/KILL, Stopped + readiness pin, group-death test) ✓; B (from_str_name + fallback_fs + BLOCK_NS, unmount_lazy + fake fail-injection + escalation pinned in disk_ops) ✓; C (park helper + non-return test, Partial + 0700 + tests) ✓; D (Makefile + README + harness reorder) ✓.
- **Honesty pins:** drain test Finished→Stopped expectation change is intentional (the new honest state); group-death via `killpg(pgid, None)` ESRCH; empty-fs STATE still formatted.
- **Adapt points flagged:** tokio `process_group` availability (fallback pre_exec setsid); `emergency` module path/behavior.
- **Placeholder scan:** none.
