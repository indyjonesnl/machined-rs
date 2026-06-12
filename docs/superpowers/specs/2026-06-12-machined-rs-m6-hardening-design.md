# machined-rs M6 — Hardening Sweep, Design

**Date:** 2026-06-12
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (milestone M6)
**Builds on:** M0–M5 merged to `main`. Scope = the accumulated review carry-forwards, grouped A–D.
Per-service health probes stay deferred (feature-sized, own cycle if wanted).

## 1. Overview

M6 closes the sharp edges the milestone reviews flagged but deferred: process-group termination
(grandchildren die), an honest `Stopped` state, reset wiping even corrupt volumes, lazy-unmount
escalation, PID1 never exiting after a failed final syscall, PKI directory hygiene, and a one-command
way to run the privileged test tier.

## 2. Goals (by group) / Non-goals

### A: stop/supervision
- `ProcessRunner` spawns each child in its own session (`setsid` via `pre_exec`), making the child a
  process-group leader; `stop_all` signals the **group** (`killpg(pgid, SIGTERM)`); the grace-expiry
  path also `killpg(pgid, SIGKILL)` before aborting the task (kill_on_drop remains the backstop for
  the direct child). Forking payloads' grandchildren now actually terminate.
- `ServiceState::Stopped`: when the stop intent is set, the post-run status is `Stopped`
  ("drained") regardless of exit code — a TERM-induced non-zero exit during stop no longer reads as
  `Failed`. (`DefaultReadiness`: `Stopped` is NOT ready — same as today's not-ready default for
  unknown states; it only appears during shutdown.)

### B: reset/disk
- `perform_reset`: when `VolumeStatus.fs` is empty/unknown, fall back to the fixed layout's
  per-label type (STATE/EPHEMERAL → ext4) so a corrupt-fs volume — the one most needing a wipe —
  still gets formatted.
- Dedup: reuse `machined_controllers::block::NS` (no hardcoded `"block"`); add
  `FsType::from_str_name(&str) -> Option<FsType>` in `block` (single source; `fs_type_of` in
  machined becomes a thin call or is removed).
- `SyncAndUnmount`: when plain `unmount` fails, retry with `MNT_DETACH` (lazy); warn on both.
  `Platform` gains `unmount_lazy(target)` (Linux `umount2(MNT_DETACH)`; fake records as
  `"unmount_lazy:<t>"` in `disk_ops`).

### C: PID1/PKI
- A failed `platform.reboot()/poweroff()` in any final action calls
  `emergency::enter_emergency(...)` instead of falling through to `Ok(())` — PID1 must never exit.
  (On the fake/test path `enter_emergency` must remain non-terminal/testable as it is today.)
- `NodePki::load_or_generate`: the PKI dir is created `0700`; a **partial** file set (1–3 of
  ca.pem/ca.key/server.pem/server.key) is a `PkiError::Partial` — never silent regeneration
  (which would orphan issued client certs). Empty dir → generate; full set → load; partial → error.

### D: dev-infra
- `make root-tests`: runs the gated privileged tests in one command
  (`sudo -E cargo test -p <crates> -- --ignored`), documented in the README (what it needs: root,
  loop devices, optionally a running containerd).
- `boot_harness` mirrors the production shutdown ordering (runtime-cancel-equivalent before
  `shutdown_sequence`) so the M5b reorder is exercised by a test, not just by inspection.

### Non-goals
- Per-service health probes (HTTP/exec); cgroups; API authz tiers; cert rotation; secure erase;
  hotplug re-discovery guards (no hotplug exists); upgrade/kexec.

## 3. Architecture notes

- **setsid + killpg:** `Command::process_group(0)` (tokio/std support setting pgid at spawn — use
  `process_group(0)` rather than a raw `pre_exec(setsid)`; it makes the child its own group leader
  without a new session, which suffices for `killpg`). The pid slot value doubles as the pgid
  (leader's pid == pgid). `stop_all`: `killpg(pgid, TERM)`; on grace expiry `killpg(pgid, KILL)`
  then abort. ESRCH ignored on both.
- **Stopped publication:** `run_supervised` knows the stop intent; after `run_service` returns with
  the intent set, it overwrites the final status with `Stopped`/"drained". (`run_service` itself
  stays attempt-honest; the supervision loop owns the stop semantics.)
- **Emergency wiring:** `enter_emergency(&platform, &err, …)` already exists from M1 (used at boot
  failure); the final-action arms reuse it. The existing signature/behavior is kept.
- **PKI partial:** `PkiError::Partial(Vec<String>)` (missing file names) — explicit, actionable.

## 4. Testing strategy

- **A:** real-process test: service `sh -c 'sleep 30 & echo started; trap "kill $!; exit 0" TERM; wait'`
  spawning a grandchild — after `stop_all`, BOTH the sh and the grandchild sleep are gone (pin via
  `kill(pid, 0)` == ESRCH for both, reading the grandchild pid from the sh's stdout is overkill —
  instead use pgid: assert `killpg(pgid, 0)` errors ESRCH after stop). `Stopped` pinned in the
  drain test (was `Finished`) + readiness table extended (`Stopped` → not ready).
- **B:** unit: fallback mapping (empty fs + STATE → ext4 format issued; unknown label + empty fs →
  skip); `FsType::from_str_name` table; sequencer test: fake whose plain unmount fails once →
  `unmount_lazy` recorded (escalation pinned in `disk_ops`).
- **C:** unit: partial PKI dir (delete server.key, reload → `Partial` error listing it; full set
  loads; empty generates); dir mode 0700 asserted. machined: a fake-platform test of the
  final-action helper — reboot Err → emergency recorded (the M1 fake records emergency via
  `enter_emergency`'s observable effect; if none exists, assert via the platform's recorded state or
  refactor `enter_emergency` minimally to be observable — flag in plan).
- **D:** `make root-tests` exists + documented; harness reorder keeps the suite green.

## 5. Key risks

- **`process_group(0)` portability/availability** — stable on Unix std/tokio `Command` (Rust 1.64+).
  Fallback if unavailable in the resolved tokio: `unsafe pre_exec(setsid)`. Plan flags it.
- **killpg on the fake path / tests without group** — tests that don't fork still work (group of
  one).
- **`Stopped` semantics drift** — only the supervision loop publishes `Stopped` (single writer
  preserved); readiness explicitly not-ready.
- **Lazy unmount hides real failures** — both attempts warn; `disk_ops` makes escalation visible.
- **PKI Partial breaking idempotent boot** — full-set load path unchanged; only genuinely partial
  dirs (previously silently re-keyed!) now error. That's the point.
