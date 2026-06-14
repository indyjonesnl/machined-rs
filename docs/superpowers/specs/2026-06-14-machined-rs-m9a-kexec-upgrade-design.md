# M9a — Atomic OS Upgrade via kexec (mechanism) (design)

**Date:** 2026-06-14
**Status:** Approved (design); M9 decomposed into M9a (this) + M9b (rollback/A-B).
**Builds on:** the whole stack — machined as PID1, the NodeAction/FinalAction plumbing, the shutdown sequence, STATE/PKI persistence.

## Goal

machined upgrades the running OS by downloading a new image bundle, verifying it, and
**kexec**-ing into the new kernel+initramfs — with STATE/PKI surviving the warm boot. The x86_64
boot test proves a v1→v2 upgrade: the node comes back up reporting the new image identity, with
its persistent volumes + CA intact. This is the headline immutable-OS capability (atomic,
machined-native updates with no bootloader).

## Decomposition (M9 is large)

| | Scope | Survives cold reboot? | Proves |
|---|---|---|---|
| **M9a** (this) | download → verify → kexec into the new image | **No** (in-memory) | the kexec mechanism + image identity + STATE/PKI persistence across the warm boot |
| **M9b** | persist the upgrade to disk (A-B slots) + health-gated rollback | Yes | a broken upgrade auto-recovers; the upgrade survives a power cycle |

## The qemu/kexec reality (drives the design)

qemu's `-kernel`/`-initrd` are fixed at launch, and the kexec'd kernel+initramfs live only in RAM.
The on-disk `/boot` (and qemu's external `-kernel`) stay v1. Therefore:
- A **kexec** swaps the running kernel+initramfs → the node is v2 **in memory**.
- A **cold reboot** returns to v1 (qemu reloads its external `-kernel`; on real hardware, firmware
  reloads the disk's `/boot`).

M9a is the **in-memory kexec upgrade** — fully qemu-testable. Persisting the new image to disk so a
cold reboot also boots v2 is **M9b** (and, like the Pi, partly hardware-verified). This split keeps
M9a's proof entirely within the qemu boot-test.

A kexec'd boot is **already idempotent** with a cold boot: machined re-runs `run_daemon`, re-mounts
STATE, and `seed_pki`/`load_or_generate` are idempotent (STATE's existing CA wins over `/boot/pki`).
So no special "post-kexec" boot path is needed — the new image just boots normally and finds the
same STATE.

---

## Components

### 1. Image identity marker (`crates/imager` + `crates/machined`)

The marker MUST live in the **initramfs rootfs** (what kexec replaces), NOT on `/boot` (the on-disk
boot partition stays v1 across a kexec).

- **Imager**: a new `--image-id <str>` CLI flag (and `BuildOpts.image_id`); the build writes the
  string to `/etc/machined/image-id` inside the initramfs rootfs before the cpio is assembled.
  Default when unset: `"dev"`.
- **machined**: reads `/etc/machined/image-id` at startup (absent → `"unknown"`), and the apiserver
  `Version` RPC response gains an `image_id` field. `machinectl version` prints it. v1 initramfs
  reports `image-id=v1`; after kexec into v2's initramfs, `image-id=v2`.

This is a genuinely useful node property ("what image am I running") independent of the upgrade test.

### 2. Upgrade bundle format

A `.tar.gz` whose root contains exactly `vmlinuz` + `initramfs.img` (the imager's existing
`emit_boot` output, tarred). machined downloads the bundle, verifies its sha256, and extracts the
two files to `/var/machined-upgrade/`. (No new imager emit mode — the operator/test tars the
`--emit-boot` directory; the format is documented.)

### 3. `Upgrade` action + `UpgradeStatus` resource

- **`NodeAction::Upgrade { url: String, sha256: String }`** (`crates/apiserver`): new variant; an
  `Upgrade` gRPC RPC (`UpgradeRequest { url, sha256 }`) sends it to the action channel.
- **`machinectl upgrade <url> <sha256>`** subcommand → the `Upgrade` RPC.
- **`UpgradeStatus { phase: UpgradePhase, message: String }`** — a new closed-enum resource
  (namespace `runtime`), `UpgradePhase ∈ { Downloading, Verifying, Loaded, Failed }`. machined
  publishes it as the upgrade progresses (and on failure with a reason), so the operator + the
  boot-test can observe an upgrade via `machinectl get UpgradeStatus`.

### 4. Graceful prepare-then-fire flow (`crates/machined/src/main.rs`)

The safety property: **a bad upgrade must not take the node down.** The main loop restructures so
`Upgrade` runs a **prepare** step BEFORE committing to shutdown:

1. `UpgradeStatus=Downloading` → HTTP(S) GET the bundle to `/var/machined-upgrade/bundle.tgz`.
2. `UpgradeStatus=Verifying` → sha256 the bundle; mismatch → fail.
3. Extract `vmlinuz` + `initramfs.img` to `/var/machined-upgrade/`.
4. `platform.kexec_load(vmlinuz, initramfs, cmdline)` — load into the kernel's kexec buffer while
   `/var` is still mounted. `UpgradeStatus=Loaded`.

If **any** step errors → `UpgradeStatus=Failed { message }`, log, and **keep running** (re-enter the
action wait; the node stays on the current image). Only a successful load proceeds to:

5. The normal shutdown sequence (stop services, sync, unmount — the kexec image is already in the
   kernel buffer, so unmounting `/var` is safe).
6. `platform.reboot_kexec()` → jump into the new kernel.

This requires turning the current single `select! → shutdown` into a **loop**: terminal actions
(Stop/Reboot/Poweroff/Reset, or a successfully-prepared Upgrade) break to shutdown; a failed Upgrade
prepare continues the loop. The download/verify/load run on the main task (the API server keeps
serving on its own task throughout, so `UpgradeStatus` is queryable during the upgrade).

### 5. kexec Platform primitives (`crates/platform`)

Two new privileged `Platform` trait methods (no default; both impls update):

```rust
/// Load a new kernel+initramfs into the kexec buffer (kexec_file_load(2)).
/// `cmdline` is reused verbatim — typically the current /proc/cmdline.
fn kexec_load(&self, kernel: &Path, initrd: &Path, cmdline: &str) -> Result<()>;
/// Boot the previously-loaded kexec image (reboot(LINUX_REBOOT_CMD_KEXEC)).
fn reboot_kexec(&self) -> Result<()>;
```

- **`LinuxPlatform`**: `kexec_load` opens both files and calls `kexec_file_load(kernel_fd, initrd_fd,
  cmdline.len()+1, cmdline_cstr, 0)` via a raw `libc::syscall(SYS_kexec_file_load, ...)`;
  `reboot_kexec` calls `libc::reboot(libc::LINUX_REBOOT_CMD_KEXEC)`. Errors → `PlatformError::Other`.
- **`FakePlatform`**: records `kexec_loaded: Option<(kernel, initrd, cmdline)>` + `reboot_kexec`
  bool, so the sequencer/daemon logic is testable without a real kexec.

The cmdline comes from `platform.kernel_cmdline()` (the running `/proc/cmdline`), so the kexec'd
kernel keeps the same console etc.

### 6. Download + verify

A minimal HTTP(S) client with sha256 verification — crate chosen at plan time, favoring a small
footprint (machined is PID1 on a 512 MB node). Mirror or reuse the imager's existing fetch posture
(pinned-by-sha). For the boot test, plain HTTP over slirp (`http://10.0.2.2:<port>/…`) suffices;
HTTPS support is desirable but the asserted path is HTTP.

### 7. Boot-test (`scripts/boot-test-x86_64.sh`, x86 only)

After the existing single-boot asserts (API up, volumes Provisioned, RuntimeReady, pods Running),
extend to prove the upgrade:

1. Build a **v2** image with `--image-id v2` (the normal build is v1 via `--image-id v1`), emitting
   its `vmlinuz`+`initramfs.img`. `tar czf bundle.tgz -C boot-v2 vmlinuz initramfs.img`; `sha256sum`.
2. Serve the host dir containing `bundle.tgz` via `python3 -m http.server <port>` (background);
   the guest reaches it at `http://10.0.2.2:<port>/bundle.tgz`.
3. Assert v1: `machinectl version` shows `image-id=v1`.
4. `ctl upgrade http://10.0.2.2:<port>/bundle.tgz <sha256>`.
5. Wait for the API to drop (kexec) then answer again; assert:
   - `machinectl version` → `image-id=v2` (the kexec booted the new initramfs), AND
   - STATE + EPHEMERAL still `phase=Provisioned`, AND `RuntimeStatus ready=true`, AND
   - the **same** machinectl client bundle still authenticates (STATE's CA persisted across the warm
     boot — also partially closes the warm-reboot-CI carry-forward).

The KVM-fast x86 job runs this; aarch64/Pi keep their current bars.

### 8. Out of scope (→ M9b or later)

Disk persistence / cold-reboot survival; A-B slots; health-gated rollback (a broken kexec'd image
leaves the node down until a cold reboot, which returns to v1); signature/PKI verification of the
bundle (sha256 only); upgrading the on-disk `/boot`; downgrade protection; concurrent-upgrade
guarding beyond the single action channel.

---

## Risks / watch-outs

- **`CONFIG_KEXEC_FILE`**: `kexec_file_load(2)` needs it set in the Alpine `linux-virt` kernel.
  Verify empirically (the overlay/module precedent — a real boot-test kexec). If absent (ENOSYS) or
  the kernel enforces `KEXEC_SIG`, the documented fallback is staging the `kexec` userspace binary
  (kexec-tools apk) and shelling `kexec -l … && kexec -e`. Confirm at plan/boot-test time.
- **kexec under qemu/KVM**: kexec should work under KVM; if the x86 KVM path misbehaves, the TCG
  fallback still exercises it. The boot-test is the arbiter.
- **Load-before-unmount ordering**: `kexec_load` MUST run while `/var` is mounted (it reads the
  staged fds); the fire (`reboot_kexec`) runs after the shutdown sequence unmounts. The
  prepare-then-fire split enforces this.
- **Main-loop restructure**: turning the one-shot `select!` into a loop that can survive a failed
  upgrade is the largest behavioral change; cover it with the existing FakePlatform-based daemon
  tests + the new failure-path assertion (`UpgradeStatus=Failed`, node still up).
- **HTTP client footprint**: keep the new dependency small; a failed/slow download must time out and
  leave the node running (the graceful-abort property).
- **`/var` (EPHEMERAL) availability**: the staging dir needs EPHEMERAL mounted; if an upgrade is
  requested before EPHEMERAL is up, fail gracefully (`UpgradeStatus=Failed`).
