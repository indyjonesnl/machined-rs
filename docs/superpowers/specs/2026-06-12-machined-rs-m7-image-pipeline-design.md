# machined-rs M7 — Bootable Image Pipeline, Design

**Date:** 2026-06-12
**Status:** Approved (brainstorming) — proceeds to implementation plan(s)
**Parent design:** `2026-06-10-machined-rs-design.md` (deferred item: image pipeline)
**Builds on:** M0–M6 merged to `main` (full lifecycle works in the test suite; no bootable artifact exists).

## 1. Overview

M7 makes machined actually boot: a new `machined-imager` crate builds a flashable disk image,
entirely in userspace (no root, no loop devices), for **both** x86_64 and aarch64 (Raspberry Pi).
The image is **full stack**: machined as PID 1 plus containerd+runc, so the README's node YAML
runs on real hardware. CI gains a QEMU boot test that asserts the node comes up end-to-end
(API over mTLS, runtime ready, disks provisioned).

Brainstorm decisions (user): both arches in M7 · kernel+initramfs with machined=`/init` ·
prebuilt pinned kernels · "whatever Raspberry Pi supports" for boot · Rust imager crate ·
config.yaml on the FAT boot partition · full stack (containerd+runc) · QEMU boot test in CI.

## 2. Boot model

**No traditional bootloader on either arch.** Firmware loads kernel+initramfs directly:

- **aarch64 (Pi):** Pi firmware reads the FAT partition: `config.txt` (names kernel + initramfs),
  `cmdline.txt`, `kernel8.img`, dtbs/overlays, firmware blobs. Self-bootable from SD card.
- **x86_64:** CI and dev boot the same image via QEMU `-kernel/-initrd/-append` (direct kernel
  boot; KVM is available on GitHub runners). **x86 bare-metal self-boot (UKI/sd-boot) is a
  non-goal in M7** — the disk image, initramfs, and code paths are identical, only the kernel
  hand-off differs.

**Initramfs is the OS.** A small cpio: static-musl `machined` as `/init`, plus `mkfs.ext4` with its
musl libs from pinned Alpine apks (the block backend shells out to mkfs), and the kernel-module
subset the target needs (Alpine builds virtio/ext4/vfat as modules; the imager resolves the
modules.dep closure and machined loads the list via `finit_module` at early boot — no busybox, no
shell, no modprobe). It stays resident in RAM, so it must stay small (target < 20 MB compressed) —
which is why containerd does NOT live here. *Plan-time amendments:* the management API binds
`0.0.0.0:50000` (QEMU/real NICs deliver to the NIC address, not loopback; mTLS authenticates every
connection); unseeded image boots re-key PKI each boot because PKI setup precedes the STATE mount —
the `--pki-dir` seed avoids it, and the ordering fix lands in M7b.

**Disk layout in the image:** GPT + one FAT partition (label `EFI`) only. STATE and EPHEMERAL are
**not in the image** — machined provisions them on first boot. *Plan-time amendment:* the existing
guard demands exact `{EFI,STATE,EPHEMERAL}` equality, so a flashed image would be `RefuseForeign`;
M7a adds a fourth guarded decision, **CompleteLayout** (disk carries exactly one partition, labeled
`EFI` → append STATE+EPHEMERAL into free space — sized to the real disk — and format only the new
partitions; EFI never re-partitioned or formatted, pinned by tests; explicit `wipe: true` still
outranks adoption).

**FAT boot partition contents:**

```
config.yaml                  # machine config — editable from any laptop
kernel8.img / vmlinuz        # pinned prebuilt kernel (per arch)
initramfs.cpio.zst
config.txt, cmdline.txt, *.dtb, firmware blobs   # aarch64 only
bin/containerd  bin/runc  etc/containerd-config.toml
```

containerd+runc exec from `/boot/bin` (FAT mounts exec by default; containerd needs no symlinks).
~80 MB stays on disk instead of RAM — decisive on the 512 MB target.

## 3. Components

### 3.1 `crates/imager` (`machined-imager`, CLI binary; not part of the node)

`machined-imager --arch {x86_64|aarch64} --config node.yaml --out machined-<arch>.img`

- **Artifact manifest** (`artifacts.toml`, committed): per-arch pinned URLs + sha256 for kernel
  (Pi: raspberrypi/firmware kernel8.img + dtbs + blobs; x86: Alpine `linux-virt` vmlinuz [+ virtio
  modules if not built in — verified at plan time]), containerd + runc release tarballs,
  busybox-static, static e2fsprogs mkfs. Downloads cached under `target/imager-cache/<sha256>`;
  checksum mismatch = hard error.
- **Initramfs builder:** pure-Rust cpio (newc) writer + zstd; injects the cross-compiled machined
  binary as `/init`, mkfs tools, minimal `/dev`,`/proc`,`/sys` dirs.
- **Image builder:** `gpt` crate (existing dependency) writes the partition table; `fatfs` crate
  populates the FAT filesystem — fully userspace.
- Embeds the user-supplied `config.yaml` onto FAT (validated by `machined-config` parse first).

### 3.2 machined changes (small)

- **New early-boot step** (sequencer, before config load): locate the GPT partition labeled `EFI`
  on any disk, mount it at `/boot` (vfat), read config from `/boot/config.yaml`. Fallback to
  `/etc/machined/config.yaml` keeps every existing test and dev flow working. Fake platform
  coverage as usual.
- Note: this makes `/boot` mounted **before** the M2b mount controller runs; the mount controller
  must treat already-mounted `/boot` as success (it checks `/proc/mounts` — verify idempotence).

### 3.3 Cross-compilation

Both arches build as static binaries: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`.
Tooling (cargo-zigbuild vs cross-rs vs plain musl toolchain) chosen at plan time; constraint:
must work locally and in CI, and `tonic`/`prost` (vendored protoc) must build for musl.

### 3.4 Full-stack node config (shipped example)

`examples/node-full.yaml`: containerd as built-in runtime service (`/boot/bin/containerd`,
config from `/boot/etc/containerd-config.toml`, health-gated via existing CRI probe) + the
install section. Payload service slot documented but empty by default (rusternetes is the
reference, not a dependency).

## 4. CI boot test

New workflow job (x86_64): build musl machined → `machined-imager --arch x86_64` → boot QEMU
(KVM, 512 MB RAM to honor the target, virtio disk+net, user-mode net with port-forward 50000) →
poll the mTLS API → assert:

1. `machinectl version` answers over mTLS. Normally PKI is generated on first boot inside the
   guest at `/system/state/pki` — unreachable from the host. So: imager gains optional
   `--pki-dir` that copies a pre-generated CA + server identity to `pki/` on the FAT partition;
   machined, when `/system/state/pki` is empty AND `/boot/pki` exists, seeds from it (copy, then
   enforce 0700/0600 — FAT carries no unix perms) before `load_or_generate` (which then loads,
   never re-keys, per the M6 `PkiError::Partial` rule). The test keeps the matching client
   bundle. `--pki-dir` is also the documented way for operators to pre-provision trust.
2. `RuntimeStatus ready=true` (containerd actually ran from `/boot/bin`).
3. `DiskStatus`/`VolumeStatus` show STATE+EPHEMERAL provisioned and mounted.

Hard timeout ~3 min; QEMU serial log uploaded as artifact on failure.

## 5. Build order (one spec, three plans)

- **M7a — imager + x86_64 boot:** crate, manifest, initramfs, image, early-boot `/boot` config
  step, QEMU CI test with machined-only config (no containerd yet). Proves PID 1 on a real kernel.
- **M7b — full stack:** containerd+runc on FAT, full-stack config, CI asserts RuntimeReady.
- **M7c — aarch64/Pi:** cross-build, Pi firmware layout, SD-card image. Verification is manual on
  the user's hardware (documented checklist); CI builds the image but cannot boot it.

## 6. Non-goals

- x86_64 bare-metal self-boot (UKI/systemd-boot) — image boots via QEMU direct kernel boot only.
- A/B upgrade, kexec, image signing/secure boot.
- CNI plugins / kubelet bundling — payload remains the operator's choice.
- Custom kernel builds — prebuilt pinned kernels only (size optimization later).

## 7. Risks

- **Prebuilt kernel config gaps** (virtio, ext4, vfat, devtmpfs built-in vs modules): verified
  first thing in M7a; fallback = ship the few needed `.ko` files in the initramfs with busybox
  `modprobe`.
- **QEMU-in-CI flakiness:** KVM on GitHub runners is supported but the job gets a hard timeout,
  serial-log artifact, and is required only after it proves stable.
- **containerd dynamic linking:** official release binaries are Go-static in practice; verified
  by checksum-pinned artifact + `RuntimeReady` CI assert (M7b).
- **Pi firmware quirks** (initramfs `ramfsfile` config, 512 MB Pi 3A+ memory split): manual M7c
  verification; documented.

## 8. Testing

- imager unit tests: cpio golden output, GPT layout asserts (read back with `gpt` crate), FAT
  tree listing (read back with `fatfs`), checksum-mismatch rejection, config validation gate.
- machined: early-boot `/boot` mount + config-from-boot path against the fake platform.
- Integration: the CI QEMU boot test (M7a machined-only, M7b full stack).
- Manual: Pi SD-card checklist (M7c).
