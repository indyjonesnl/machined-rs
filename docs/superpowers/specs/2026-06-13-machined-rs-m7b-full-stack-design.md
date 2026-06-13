# machined-rs M7b — Full-Stack Image (containerd + runc), Design

**Date:** 2026-06-13
**Status:** Approved (brainstorming) — proceeds to implementation plan(s)
**Parent design:** `2026-06-12-machined-rs-m7-image-pipeline-design.md` (M7 build order: M7b = full stack)
**Builds on:** M7a merged to `main` (bootable x86_64 image, QEMU boot test green in CI).

## 1. Overview

M7b makes the README's pitch true on metal: the node boots, provisions, and brings up a
**real container runtime**. containerd + runc ship as sha256-pinned static binaries on the FAT
boot partition (`/boot/bin`), executed in place; machined supervises containerd as today (M4a) and
gates payloads on CRI `RuntimeReady`. M7b also retires the one sharp edge M7a left in `main` — the
PKI/STATE-mount ordering race — and adds supervisor restart backoff so the first real
`restart: Always` service can't hot-loop.

Brainstorm decisions (user, all recommended options): pinned static release binaries to `/boot/bin`
· gate PKI/API on the STATE-mounted signal · assert `RuntimeReady=true` in CI (no test-container) ·
include restart backoff.

## 2. Build order (one spec, two plans)

- **M7b-1 — race fix + backoff.** Pure machined/supervisor changes, fake-tested, no image work.
  Mergeable on its own; removes the cold-boot-only CI constraint.
- **M7b-2 — runtime on metal.** Imager delivers containerd+runc to `/boot/bin`; cgroup mount;
  functional containerd config; PATH includes `/boot/bin`; `node-ci.yaml` enables the runtime; the
  QEMU boot test asserts `RuntimeReady=true`.

## 3. M7b-1 — PKI/STATE race fix + restart backoff

### 3.1 The race (recap)

`run_daemon` today (`crates/machined/src/main.rs`): the pid1 block calls `seed_pki` to
`/system/state/pki` **before** the STATE volume is mounted, so the seed lands on the initramfs
rootfs and is later shadowed by the STATE ext4 mount (done asynchronously by `VolumeMountController`
in the spawned controller runtime). `NodePki::load_or_generate` then races the mount: if the mount
wins, it sees an empty `/system/state/pki` and mints a **fresh CA**, locking out the baked
`machinectl` bundle. The KNOWN RACE comment marks the spot (main.rs ~lines 256–262).

On a **cold** boot STATE does not exist until `VolumeProvisionerController`'s CompleteLayout creates
it mid-boot, so the fix cannot wait on a pre-existing STATE — it must wait for the controller
runtime to provision **and** mount it.

### 3.2 Fix: gate PKI/API on the STATE-mounted signal

- New `wait_for_state_mount(state, timeout) -> bool` (machined): polls the COSI store (same cadence
  as the supervisor's `wait_for_deps`, ~200 ms) for a `MountStatus` whose target is `/system/state`
  and `mounted == true`. Returns `true` on mount, `false` on timeout.
- In `run_daemon`, **after** the controller runtime is spawned and **before** PKI work:
  - If `provider.install().is_some()` (an install disk is configured → STATE is expected), call
    `wait_for_state_mount` with a generous timeout (e.g. 60 s). Log a warning on timeout and proceed
    best-effort (don't hang PID 1 forever).
  - If no install disk (dev/test/non-image flows), **skip the wait** — behaviour is exactly as today.
- Move `seed_pki(/boot/pki → /system/state/pki)` to run **after** the wait, so it writes onto the
  mounted ext4 STATE. Keep the M7a guarantees (never overwrite existing dst; 0700/0600; atomic
  temp-dir + rename) and **add fsync** of the staged files before the rename (ext4 `auto_da_alloc`
  doesn't cover rename-to-new-path; a power cut post-rename could otherwise leave zero-length keys).
- `NodePki::load_or_generate` and the API spawn stay where they are, now strictly after the seed on
  real STATE → no fresh-CA path on warm boots.

Order in `run_daemon` becomes: pid1 mounts/modules/boot-mount → config load → controller runtime
spawn (provisions + mounts STATE/EPHEMERAL) → **wait_for_state_mount** → seed_pki → PKI load → API
spawn → boot_sequence (services).

`MountStatus` shape to wait on: `VolumeMountController` already publishes `MountStatus`
(`crates/controllers/src/block/mount.rs`); confirm the field exposing the mountpoint + mounted flag
at plan time and key the wait on the STATE row.

### 3.3 Restart backoff (supervisor)

`crates/supervisor` `run_supervised` restarts per policy with no delay — a service whose binary is
missing (observed with containerd in the M7a serial log) hot-loops at ~10 Hz. Add per-service
exponential backoff between restart attempts:

- Base delay ~1 s, doubling to a cap ~30 s.
- **Reset** the backoff once the service has been continuously Running+healthy past a threshold
  (e.g. 60 s), so a long-lived service that later crashes recovers fast rather than starting at the
  cap.
- The stop-aware wait must remain responsive (the delay sleeps in the same select! that watches the
  stop signal, so a stop during backoff is honoured promptly).
- Tests (fake clock / short delays): N rapid failures produce increasing delays; a healthy run resets
  the delay; a stop during backoff returns promptly.

## 4. M7b-2 — runtime on metal

### 4.1 Imager: deliver containerd + runc to `/boot/bin`

Two new artifact kinds (`crates/imager/src/manifest.rs` + dispatch in `build.rs`), staged into a
`staging/bin/` dir that becomes FAT `/boot/bin` — **separate** from the initramfs rootfs:

- `boot-tarball` — gunzip + untar; copy entries under `bin/` to `staging/bin/`, mode 0755. For the
  official `containerd-<ver>-linux-amd64.tar.gz` (carries `bin/containerd`,
  `bin/containerd-shim-runc-v2`, `bin/ctr`, …). Reuse the existing containment guard
  (Normal-components only) when writing.
- `boot-binary` — copy a single file to `staging/bin/<name>` (optional `rename` field:
  `runc.amd64` → `runc`), mode 0755.

`artifacts.toml` gains containerd + runc entries (pinned URL + sha256), `kind` set accordingly.
The image writer already stages `staging/` to FAT; a `bin/` subdir just rides along. Existing apk
artifacts (kernel, mkfs, modules) continue to target the initramfs unchanged.

Image-size note: ~70 MB added to the 512 MB FAT partition; initramfs unchanged (stays small).

### 4.2 cgroups + runtime environment

- **Mount cgroup2 unified** at `/sys/fs/cgroup` (fstype `cgroup2`), after sysfs. Add to the
  essential-mount path (idempotent; harmless when the runtime is disabled). The Alpine `linux-virt`
  kernel ships cgroup v2.
- If containerd/runc need controllers delegated in the root cgroup
  (`echo +cpu +memory +pids > /sys/fs/cgroup/cgroup.subtree_control`), add a minimal one-shot step;
  **verify the need empirically** in the boot test before adding (cgroupfs driver may not require it
  for the RuntimeReady bar). Documented as a plan-time risk.
- **PATH:** extend the pid1 default PATH (added in M7a) to include `/boot/bin`, so the supervised
  containerd — which inherits machined's env — finds `containerd-shim-runc-v2` and `runc`.

### 4.3 containerd config

`machined_config::containerd_config_toml()` grows from the minimal stub to a functional config for
the pinned containerd version:

- `root = /var/lib/containerd` (persistent — EPHEMERAL → `/var`), `state = /run/containerd` (tmpfs).
- `[grpc] address = <socket>` (unchanged default `/run/containerd/containerd.sock`).
- CRI plugin enabled with the `io.containerd.runc.v2` runtime, **cgroupfs** driver
  (`SystemdCgroup = false` — there is no systemd).
- Config-schema **version must match the pinned containerd 2.x** (v2 vs v3 plugin paths differ);
  pin the version and the matching schema together at plan time.

`RuntimeSection.binary` for the image is `/boot/bin/containerd` (set in `node-ci.yaml`; the default
may stay `/usr/bin/containerd` for non-image contexts). `containerd_service()` and the
`RuntimeHealthController`/`RuntimeReadiness` gate are unchanged.

### 4.4 Verification

- `examples/node-ci.yaml`: `runtime.disabled: false`, `runtime.binary: /boot/bin/containerd`.
- `scripts/boot-test-x86_64.sh`: after the API + volume assertions, add a deadline-looped check that
  `machinectl get RuntimeStatus` shows `ready=true` (containerd start + CRI plugin init takes a few
  seconds; allow ~120 s). On failure, dump the serial log (machined tracing + containerd output).
- CI boot-test job (already in the GHCR tool image) runs it unchanged; the image build now downloads
  containerd+runc (cached via the existing `target/imager-cache` Actions cache).

## 5. Non-goals

- Running an actual pod/container in CI (needs registry egress + an image — its own milestone).
- aarch64/Pi runtime (M7c).
- CNI networking, image GC, snapshotter tuning, seccomp/AppArmor profiles.
- Warm-reboot CI assertion (the race fix enables it; adding the second-boot CI pass is optional
  follow-up, not required for M7b).

## 6. Risks

- **containerd 2.x config schema** — v2/v3 plugin path differences; pin version + schema together,
  verify the CRI plugin loads (RuntimeReady) in the boot test.
- **cgroup v2 in a systemd-less PID1** — controller delegation may be needed; verify empirically,
  add the subtree_control step only if RuntimeReady requires it.
- **RuntimeReady timing** — generous deadline in the boot test; containerd+CRI aren't instant.
- **exec-from-FAT** — vfat mounts files executable by default (no unix exec bit); the `/boot` mount
  already omits `noexec` (M7a). Confirm a binary under `/boot/bin` actually execs in the guest.
- **/var/lib/containerd persistence** — depends on EPHEMERAL being mounted at `/var` before
  containerd starts; containerd is started in the `services` boot phase, after the mount controller,
  so ordering holds — confirm in the boot test.

## 7. Testing

- M7b-1: machined unit tests for `wait_for_state_mount` (mounts → returns true; timeout → false;
  no-install-disk → skipped) against the fake store; supervisor backoff tests (increasing delays,
  reset-on-healthy, prompt stop during backoff) against fakes. No image needed.
- M7b-2: imager unit tests for `boot-tarball`/`boot-binary` staging (golden tree under `bin/`, mode
  0755, containment guard); config-generation test (containerd TOML parses, has runc runtime +
  cgroupfs); the QEMU boot test asserting `RuntimeReady=true` (the integration gate).
- Manual: none new beyond the existing `make root-tests` tier.
