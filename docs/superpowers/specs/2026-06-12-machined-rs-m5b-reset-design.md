# machined-rs M5b — Reset, Design

**Date:** 2026-06-12
**Status:** Approved (brainstorming) — proceeds to one implementation plan
**Parent design:** `2026-06-10-machined-rs-design.md` (completes milestone M5's shipped scope; upgrade/kexec stays deferred pending an image pipeline)
**Builds on:** M5a (graceful shutdown) merged to `main`.

## 1. Overview

M5b adds **Reset**: an authenticated `machinectl reset --yes` drives the node through its graceful
shutdown, re-formats (mkfs) the STATE and EPHEMERAL partitions in place — partition table and EFI
untouched — and reboots. The next boot reprovisions naturally through the existing M2b path
(discovery sees the same labels → provisioner Skips → mount → fresh volumes; PKI regenerates via
`load_or_generate`). M5b also lands the M5a-flagged ordering fix: the controller runtime is cancelled
**before** the shutdown sequence, so no controller can act during unmount or the reset format.

## 2. Goals / Non-goals

### Goals
- `Reset` RPC (proto + handler enqueueing `NodeAction::Reset` — same channel as reboot/shutdown).
- `machinectl reset` with a required `--yes` guard (client-side; the server executes for any
  mTLS-authed caller, consistent with reboot/shutdown).
- machined: `FinalAction::Reset` — after the graceful sequence: read `VolumeStatus` for `STATE` +
  `EPHEMERAL` from the store, `BlockProvisioner::format(device, fs, label)` each, then reboot.
- Ordering fix: `shutdown.cancel()` + runtime join move **before** `shutdown_sequence`.

### Non-goals (deferred)
- Upgrade/kexec (image pipeline), wiping EFI or the partition table (factory reset), wiping
  user/extra partitions, secure erase/discard, `--graceful=false` style force flags, server-side
  confirmation/authz tiers.

## 3. Architecture

| crate | change |
|---|---|
| `apiserver` | proto `rpc Reset(Empty) returns (Empty)`; `NodeAction::Reset`; handler enqueues (same pattern as reboot/shutdown). |
| `machinectl` | `reset` subcommand with `--yes: bool`; refuses (non-zero exit, warning to stderr) without it; prints `reset requested`. |
| `machined` | `FinalAction::Reset`; the select maps `NodeAction::Reset → FinalAction::Reset`; ordering reorder (cancel runtime → join API → run `shutdown_sequence` → final action); `perform_reset(state, provisioner)` reads `VolumeStatus` `STATE`/`EPHEMERAL` (namespace `block`), maps `fs` string → `FsType`, formats each (errors logged; reset continues best-effort — a failed format still reboots), then `platform.reboot()`. machined gains a `machined-block` dependency (the real `SysfsBlock` provisioner on Linux, fake elsewhere/tests). |

### 3.1 The reset flow

```
machinectl reset --yes → Reset RPC → NodeAction::Reset
  → daemon select! → FinalAction::Reset
  → shutdown.cancel(); rt join; API join        (controllers can no longer act)
  → shutdown_sequence (graceful stop → sync+unmount)
  → perform_reset: format STATE, format EPHEMERAL (labels preserved)
  → platform.reboot()
next boot: discovery → labels present → provisioner Skip (is_ours) → mount fresh volumes
           PKI load_or_generate regenerates (STATE now empty)
```

**Ordering note (applies to ALL final actions, not just reset):** the runtime stops before the
sequencer runs, closing the M5a review's mount-controller-during-unmount race. The sequencer tasks
need no controllers (they use platform/services/provider directly). One behavior change: controllers
stop reconciling slightly earlier during a normal stop — acceptable (the node is going down).

### 3.2 fs-string mapping

`VolumeStatus.fs` is a string (e.g. `"ext4"`, `"vfat"`); `format` takes `FsType`. A small
`fs_type_of(&str) -> Option<FsType>` in `machined` (or `block`) maps it; an unknown string logs and
skips that volume (defensive; our volumes are always ext4/vfat).

## 4. Error handling & observability

- Missing `VolumeStatus` for STATE/EPHEMERAL (e.g. never provisioned): log + skip — reset degrades
  to a plain reboot rather than failing.
- Format failures: log, continue to the next volume, still reboot (the node must not be left half
  down; a partially-wiped node re-runs reset if needed).
- The reset request is logged at the handler and at execution.

## 5. Testing strategy

- **Unit:** `fs_type_of` mapping; machinectl `reset`/`--yes` parse tests (refusal is exit-code
  tested in the e2e).
- **apiserver integration:** `Reset` enqueues `NodeAction::Reset` (extend the existing actions test).
- **machined `perform_reset` unit/integration (root-free):** seed `VolumeStatus` STATE+EPHEMERAL in
  a store, run `perform_reset` against a `FakeBlockBackend` — assert exactly two `format` calls with
  the right device/fs/label and nothing else (no wipe, no create_partitions); missing-volume case
  degrades cleanly. (FakeBlockBackend already records formats from M2b.)
- **machinectl e2e:** extend the existing e2e — `reset` WITHOUT `--yes` exits non-zero and the server
  receives nothing; `reset --yes` delivers `NodeAction::Reset`.
- **Ordering:** the reorder is covered by the full existing suite (boot/shutdown/payload e2e) staying
  green; the sequencer needs no controllers, which the tests already prove.
- **CI:** `make pre-commit`.

## 6. Key risks

- **Formatting the wrong device** — devices come from the store's `VolumeStatus`, which only the
  provisioner (guarded by M2b-2a's conservative rules) publishes; reset never touches disks/devices
  not labeled STATE/EPHEMERAL. The fake-backed test pins exactly-two-formats.
- **Format-while-mounted** — prevented by ordering (unmount precedes reset) + the runtime being
  stopped (no re-mount possible). If unmount failed (busy), `format` on a mounted device fails and is
  logged; the node still reboots (and a post-reboot reset can retry).
- **The reorder regressing normal shutdown** — the full suite + the M5a shutdown tests gate it.
