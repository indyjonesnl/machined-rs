# M7c-1 — Generic aarch64 + CI Boot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a bootable aarch64 image (generic qemu `-M virt`) and prove it end-to-end in CI — the aarch64 machined binary + arm64 containerd boot and reach `RuntimeReady=true` under `qemu-system-aarch64`.

**Architecture:** The imager is already arch-keyed (`manifest.for_arch`) and the kernel path + virtio module set turn out to be identical for x86_64 and aarch64 `linux-virt` (qemu-virt uses virtio-pci on both). So M7c-1 is small: rename the misnamed module const to `VIRT_MODULES`, accept `--arch aarch64`, add the aarch64 artifact section (Alpine aarch64 apks + arm64 containerd/runc), cross-compile via `aarch64-unknown-linux-musl`, and add a qemu-system-aarch64 boot test (TCG, no cross-arch KVM). No arch-config table yet — that lands in M7c-2 when the Pi config actually diverges.

**Tech Stack:** Rust `aarch64-unknown-linux-musl` (cross-linked with `gcc-aarch64-linux-gnu`), Alpine aarch64 apks, containerd 2.0.9 arm64 + runc 1.4.3 arm64, `qemu-system-aarch64 -M virt` (TCG).

**Plan-time verified facts (downloaded + computed 2026-06-13 — re-verify shas by download at pin time, never trust a listed hash):**
- aarch64 `linux-virt-6.12.93-r0.apk` ships **`boot/vmlinuz-virt`** (same path as x86), kver `6.12.93-0-virt`, and `virtio_blk/virtio_net/ext4/vfat/nls_cp437/nls_iso8859-1/nls_utf8` all as `.ko.gz` (`=m`) — **the exact same module set as x86**. `virtio`/`virtio_pci` are builtin on both arches. So the x86 module-loading logic ports unchanged; `X86_64_QEMU_MODULES` is misnamed and should become `VIRT_MODULES`.
- aarch64 Alpine v3.21 apk shas: linux-virt `6816957eb3e706732e7c81534fdb27bcb17684c6f47a6a2a47bfa8d756080e57`, musl `721010e6bff908878d9c527428598661be59dde0d9f013f8431d01fd4dd16652`, e2fsprogs `c28dddb51a40a91820a9f0dcd32f19abf23c8256543d7afc4f87363b28885a66`, e2fsprogs-libs `c817ddefbfa19245cce0c62820a1eb50c771f4999232d0a1c9e57521a58508d3`, libcom_err `0798fedbc002cada8e74a7cb4cdae885e5f0c1b5fd59b15af5b2903b95d30153`, libblkid `61339f8737b05062f662cfc30c65cb34f10ccc53573846653551c66156c4aa0b`, libeconf `db25f8abaf1ec61f5dcc0ff9b55271a3f6e43037e430b0c126ac000483e17dad`, libuuid `d392ac96027cbe5ca9bc270d2aa4e99747b0eae3cf65a0087f2c6f13a7382702`. URLs: `https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/<file>`.
- `containerd-static-2.0.9-linux-arm64.tar.gz` sha `8f409c39562f11a116227e833797ab421a6ebde96f92aecd88ae0409a6bf1873` (bin/containerd is ARM aarch64 statically linked). `runc.arm64` v1.4.3 sha `633301e2e32f8a5ad54031aab4901eb00308bec677dd15faa2751e8f9dab5ca4` (matches the PGP-signed runc.sha256sum; aarch64 static-pie).
- Cross-compile: apt `gcc-aarch64-linux-gnu`; `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc`, `CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc`, `AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar`. ring builds with the gnu cross-gcc; rustc supplies the musl libc (no separate musl sysroot needed).
- qemu: apt `qemu-system-arm` gives `qemu-system-aarch64`. `-M virt -cpu max -smp 2 -m 512` direct `-kernel/-initrd` (NO UEFI/-bios needed), **`console=ttyAMA0`** (PL011 — NOT `ttyS0`), `virtio-blk` disk + `virtio-net-pci`. Cross-arch = **TCG only (no KVM)** → slow; budget a large timeout.
- Code seams (verified): `crates/imager/src/modules.rs:70` `X86_64_QEMU_MODULES`; `crates/imager/src/build.rs:88` `modules::X86_64_QEMU_MODULES` + `:89` `rootfs.join("boot/vmlinuz-virt")` (correct for BOTH arches — leave it); `crates/imager/src/main.rs:29` `value_parser = ["x86_64"]`; `examples/node-ci.yaml` is arch-neutral (virtio `/dev/vda`, slirp net) — no change. `scripts/boot-test-x86_64.sh` stays untouched; clone it for aarch64.

---

### Task 1: Arch generalization (rename const + accept --arch aarch64)

**Files:**
- Modify: `crates/imager/src/modules.rs` (rename const), `crates/imager/src/build.rs` (update ref), `crates/imager/src/main.rs` (value_parser)

- [ ] **Step 1: Rename the module const** — in `crates/imager/src/modules.rs`, rename `X86_64_QEMU_MODULES` → `VIRT_MODULES` and fix the doc comment (it's the qemu-virt/virtio set, shared by x86_64 + aarch64, not x86-specific):

```rust
/// The module roots a qemu `-M virt` (virtio) boot needs — shared by x86_64 and
/// aarch64 (both use virtio-pci; these are all `=m` in Alpine linux-virt). Block
/// + net + the filesystems machined mounts.
pub const VIRT_MODULES: &[&str] = &[
    "virtio_blk",
    "virtio_net",
    "ext4",
    "vfat",
    "nls_cp437",
    "nls_iso8859_1",
    // The kernel's vfat default iocharset is utf8 (CONFIG_FAT_DEFAULT_IOCHARSET="utf8");
    // without nls_utf8 the boot-partition mount EINVALs ("IO charset utf8 not found").
    "nls_utf8",
];
```

- [ ] **Step 2: Update the reference** — in `crates/imager/src/build.rs:88`, change `modules::X86_64_QEMU_MODULES` → `modules::VIRT_MODULES`. (Leave `rootfs.join("boot/vmlinuz-virt")` at line 89 — verified correct for the aarch64 linux-virt apk too.)

- [ ] **Step 3: Accept aarch64** — in `crates/imager/src/main.rs:29`, change `value_parser = ["x86_64"]` → `value_parser = ["x86_64", "aarch64"]`.

- [ ] **Step 4: Build + existing tests + clippy**

Run: `cargo build -p machined-imager && cargo test -p machined-imager && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`
Expected: clean — the rename compiles (grep confirms no other `X86_64_QEMU_MODULES` references: `grep -rn X86_64_QEMU_MODULES crates/` returns nothing), all existing imager tests pass (the module-resolver tests use their own fixtures, not the const), clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/imager/src/modules.rs crates/imager/src/build.rs crates/imager/src/main.rs
git commit -m "refactor(imager): VIRT_MODULES (shared x86/aarch64) + accept --arch aarch64"
```

---

### Task 2: Pin aarch64 artifacts

**Files:**
- Modify: `crates/imager/artifacts.toml`, `crates/imager/src/manifest.rs` (extend the real-manifest test)

- [ ] **Step 1: Re-verify the shas by download** (you have network — never trust the listed hashes)

```bash
cd /tmp && mkdir -p m7c && cd m7c
base=https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64
for f in linux-virt-6.12.93-r0 musl-1.2.5-r11 e2fsprogs-1.47.1-r1 e2fsprogs-libs-1.47.1-r1 \
         libcom_err-1.47.1-r1 libblkid-2.40.4-r1 libeconf-0.6.3-r0 libuuid-2.40.4-r1; do
  curl -fsSL -o "$f.apk" "$base/$f.apk" && echo "$(sha256sum "$f.apk")"
done
# arm64 runtime
curl -fsSL -o containerd-arm64.tgz https://github.com/containerd/containerd/releases/download/v2.0.9/containerd-static-2.0.9-linux-arm64.tar.gz
sha256sum containerd-arm64.tgz; tar -tzf containerd-arm64.tgz | head; mkdir cd && tar -xzf containerd-arm64.tgz -C cd && file cd/bin/containerd  # ARM aarch64, statically linked
curl -fsSL -o runc.arm64 https://github.com/opencontainers/runc/releases/download/v1.4.3/runc.arm64
sha256sum runc.arm64; file runc.arm64  # ARM aarch64, static-pie
curl -fsSL https://github.com/opencontainers/runc/releases/download/v1.4.3/runc.sha256sum | grep 'runc.arm64$'
```

If any Alpine apk filename 404s (Alpine rolled the `-rN`/patch within v3.21), list the dir (`curl -sL $base/ | grep -oE '<pkg>-[0-9][^"]*\.apk' | sort -uV | tail -1`), use the current file, and re-verify. Confirm both runtime binaries are ARM aarch64 + static via `file`.

- [ ] **Step 2: Add the aarch64 section** to `crates/imager/artifacts.toml`, after the x86_64 array (use the COMPUTED shas; expected values from research below — replace if your download differs):

```toml

aarch64 = [
  { name = "linux-virt", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/linux-virt-6.12.93-r0.apk", sha256 = "6816957eb3e706732e7c81534fdb27bcb17684c6f47a6a2a47bfa8d756080e57", kind = "apk" },
  { name = "musl", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/musl-1.2.5-r11.apk", sha256 = "721010e6bff908878d9c527428598661be59dde0d9f013f8431d01fd4dd16652", kind = "apk" },
  { name = "e2fsprogs", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/e2fsprogs-1.47.1-r1.apk", sha256 = "c28dddb51a40a91820a9f0dcd32f19abf23c8256543d7afc4f87363b28885a66", kind = "apk" },
  { name = "e2fsprogs-libs", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/e2fsprogs-libs-1.47.1-r1.apk", sha256 = "c817ddefbfa19245cce0c62820a1eb50c771f4999232d0a1c9e57521a58508d3", kind = "apk" },
  { name = "libcom_err", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/libcom_err-1.47.1-r1.apk", sha256 = "0798fedbc002cada8e74a7cb4cdae885e5f0c1b5fd59b15af5b2903b95d30153", kind = "apk" },
  { name = "libblkid", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/libblkid-2.40.4-r1.apk", sha256 = "61339f8737b05062f662cfc30c65cb34f10ccc53573846653551c66156c4aa0b", kind = "apk" },
  { name = "libeconf", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/libeconf-0.6.3-r0.apk", sha256 = "db25f8abaf1ec61f5dcc0ff9b55271a3f6e43037e430b0c126ac000483e17dad", kind = "apk" },
  { name = "libuuid", url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/aarch64/libuuid-2.40.4-r1.apk", sha256 = "d392ac96027cbe5ca9bc270d2aa4e99747b0eae3cf65a0087f2c6f13a7382702", kind = "apk" },
  # GitHub-release static arm64 binaries → /boot/bin:
  { name = "containerd", url = "https://github.com/containerd/containerd/releases/download/v2.0.9/containerd-static-2.0.9-linux-arm64.tar.gz", sha256 = "8f409c39562f11a116227e833797ab421a6ebde96f92aecd88ae0409a6bf1873", kind = "boot-tarball" },
  { name = "runc", url = "https://github.com/opencontainers/runc/releases/download/v1.4.3/runc.arm64", sha256 = "633301e2e32f8a5ad54031aab4901eb00308bec677dd15faa2751e8f9dab5ca4", kind = "boot-binary", rename = "runc" },
]
```

- [ ] **Step 3: Extend the real-manifest test** — in `crates/imager/src/manifest.rs`, the test `real_artifacts_manifest_parses` already loads the committed `artifacts.toml` and asserts x86_64. Add aarch64 assertions:

```rust
    // aarch64 section present with the same shape (apk kernel + arm64 runtime).
    let arm = m.for_arch("aarch64").expect("aarch64 arch present");
    assert!(arm.iter().any(|a| a.name == "linux-virt" && a.kind == "apk"));
    assert!(arm.iter().any(|a| a.name == "containerd" && a.kind == "boot-tarball"));
    assert!(arm.iter().any(|a| a.name == "runc" && a.kind == "boot-binary" && a.rename.as_deref() == Some("runc")));
```

- [ ] **Step 4: Run + gates**

Run: `cargo test -p machined-imager real_artifacts && cargo test -p machined-imager && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`
Expected: the manifest test parses the real file with both arches; all imager tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/imager/artifacts.toml crates/imager/src/manifest.rs
git commit -m "feat(imager): pin aarch64 artifacts (Alpine apks + arm64 containerd/runc)"
```

---

### Task 3: Cross-compile target (make dist-aarch64)

**Files:**
- Modify: `Makefile`

- [ ] **Step 1: Add the dist-aarch64 target** — mirror `dist-x86_64`'s fallback chain but for `aarch64-unknown-linux-musl` with the gnu cross-gcc. Add to `.PHONY` and after `dist-x86_64`:

```makefile
# Static machined for aarch64 images (cross-linked with the gnu aarch64 gcc;
# rustc supplies the musl libc, so no musl sysroot needed — ring builds with
# aarch64-linux-gnu-gcc as CC).
dist-aarch64:
	@if command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then \
		CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc \
		CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc \
		AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar \
		cargo build --release --target aarch64-unknown-linux-musl -p machined; \
	else \
		echo "FATAL: aarch64-linux-gnu-gcc not found (apt install gcc-aarch64-linux-gnu)"; \
		exit 1; \
	fi
```

Update the `.PHONY` line to include `dist-aarch64` (and `boot-test-aarch64`, added in Task 4).

- [ ] **Step 2: Verify the cross-build locally** (requires the toolchain + target; if absent, the CI image adds them in Task 5 — but try locally first)

```bash
rustup target add aarch64-unknown-linux-musl
command -v aarch64-linux-gnu-gcc || echo "NOTE: install gcc-aarch64-linux-gnu (needs sudo) — else this verifies in the rebuilt CI image at Task 6"
make dist-aarch64 2>&1 | tail -20
# If it built:
file "${CARGO_TARGET_DIR:-target}/aarch64-unknown-linux-musl/release/machined"   # expect "ELF 64-bit ... ARM aarch64 ... statically linked"
```

If `aarch64-linux-gnu-gcc` isn't installed locally (no sudo) and the build can't run, that's acceptable — Task 5 bakes the toolchain into the CI image and Task 6 verifies the cross-build there. Note in the report whether it built locally or deferred to Task 6. Do NOT mark this task done on a build you couldn't run — if deferred, say so explicitly and the reviewer/Task 6 confirms.

- [ ] **Step 3: Commit**

```bash
git add Makefile
git commit -m "build: dist-aarch64 (aarch64-unknown-linux-musl, gnu cross-gcc)"
```

---

### Task 4: aarch64 boot-test script

**Files:**
- Create: `scripts/boot-test-aarch64.sh` (clone of the x86 script with aarch64 bits; chmod +x)
- Modify: `Makefile` (boot-test-aarch64 target)

- [ ] **Step 1: Clone + adapt the script** — copy `scripts/boot-test-x86_64.sh` to `scripts/boot-test-aarch64.sh` and change ONLY the arch-specific bits (keep the assertion logic — API wait, VolumeStatus, RuntimeStatus — byte-identical so the proven assertions don't drift). The diffs:

```bash
# header/vars:
IMG=$WORK/machined-aarch64.img
MACHINED=$TARGET_DIR/aarch64-unknown-linux-musl/release/machined
TIMEOUT=${BOOT_TEST_TIMEOUT:-600}   # TCG (no cross-arch KVM) is slow — generous

# tool check:
command -v qemu-system-aarch64 >/dev/null || { echo "FATAL: qemu-system-aarch64 not installed"; exit 2; }

# build invocation:
"$IMAGER" build --arch aarch64 --machined "$MACHINED" \
  --config examples/node-ci.yaml --pki-dir "$WORK/pki" \
  --out "$IMG" --emit-boot "$WORK/boot" --cache target/imager-cache

# QEMU invocation (replaces the x86 qemu block; NO KVM, machine virt, PL011 console):
qemu-system-aarch64 -machine virt -cpu max -smp 2 -m 512 \
  -kernel "$WORK/boot/vmlinuz" -initrd "$WORK/boot/initramfs.img" \
  -append "console=ttyAMA0" \
  -drive file="$IMG",if=virtio,format=raw \
  -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${PORT}-:50000" \
  -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$SERIAL" &
QEMU=$!
trap 'kill $QEMU 2>/dev/null || true; wait $QEMU 2>/dev/null || true' EXIT
```

Keep everything else identical to the x86 script: `TARGET_DIR=${CARGO_TARGET_DIR:-target}`, `WORK=target/boot-test`, `SERIAL`, `PORT`, the `ctl()` helper, the API-wait loop, the VolumeStatus loop, the RuntimeStatus `ready=true` loop (bump its `rt_deadline` to `$((SECONDS + 300))` for TCG), the serial-tail-on-failure, the QEMU-death checks. Remove the `KVM_FLAG` logic (no KVM cross-arch). `chmod +x scripts/boot-test-aarch64.sh`.

- [ ] **Step 2: Makefile target**

```makefile
# Build the aarch64 image + boot it in qemu-system-aarch64 (TCG), assert the node comes up.
boot-test-aarch64: dist-aarch64
	cargo build --release -p machined-imager -p machinectl
	./scripts/boot-test-aarch64.sh
```

- [ ] **Step 3: Syntax check**

Run: `bash -n scripts/boot-test-aarch64.sh`
Expected: clean. (The full run happens in Task 6 against the rebuilt CI image — qemu-system-aarch64 isn't on the dev host yet.)

- [ ] **Step 4: Commit**

```bash
git add scripts/boot-test-aarch64.sh Makefile
git commit -m "test(boot): aarch64 qemu-system-aarch64 -M virt boot test (TCG)"
```

---

### Task 5: CI tool image gains aarch64

**Files:**
- Modify: `ci/Dockerfile`

- [ ] **Step 1: Add the aarch64 toolchain + qemu** — extend `ci/Dockerfile`:

```dockerfile
# apt: add qemu-system-arm (provides qemu-system-aarch64) + the aarch64 cross C
# compiler (ring's C bits cross-compile with it; rustc supplies the musl libc).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      qemu-system-x86 \
      qemu-system-arm \
      musl-tools \
      gcc-aarch64-linux-gnu \
      make \
 && rm -rf /var/lib/apt/lists/*

# rustup: add the aarch64 musl target alongside x86_64.
RUN rustup toolchain install stable --profile minimal \
      --component rustfmt --component clippy \
      --target x86_64-unknown-linux-musl \
      --target aarch64-unknown-linux-musl \
 && rustup default stable

# sanity: fail the image build if any tool is missing.
RUN qemu-system-x86_64 --version \
 && qemu-system-aarch64 --version \
 && musl-gcc --version \
 && aarch64-linux-gnu-gcc --version \
 && rustup target list --installed | grep -qx x86_64-unknown-linux-musl \
 && rustup target list --installed | grep -qx aarch64-unknown-linux-musl
```

(Merge with the existing Dockerfile structure — it has the apt block, the rustup block, and a sanity block; extend each rather than duplicating. Update the `LABEL …image.description` to mention aarch64.)

- [ ] **Step 2: Build the image locally** (validates the Dockerfile + gives Task 6 its environment)

Run: `docker build -t machined-ci:local ci/`
Expected: builds clean; the sanity layer prints both qemu versions, musl-gcc, aarch64-linux-gnu-gcc, and confirms both rustup targets.

- [ ] **Step 3: Commit**

```bash
git add ci/Dockerfile
git commit -m "ci(image): add qemu-system-aarch64 + aarch64-musl cross toolchain"
```

---

### Task 6: Local aarch64 boot validation (the integration proof)

**Files:** none (validation only; may bump a timeout in `scripts/boot-test-aarch64.sh` if TCG needs it)

- [ ] **Step 1: Run the full aarch64 boot test in the rebuilt image** (TCG — slow; allow ~15-20 min wall)

```bash
cd /home/jones/PhpstormProjects/machined-rs
docker run --rm -v "$PWD":/work -w /work -e BOOT_TEST_TIMEOUT=900 machined-ci:local \
  bash -c 'make boot-test-aarch64' 2>&1 | tee /tmp/m7c1-boot.log | tail -50
```

(Note: NO `--device /dev/kvm` — cross-arch is TCG; KVM would be ignored anyway.) Also confirms `make dist-aarch64` cross-builds in the image (Task 3's deferred verification): the log shows the aarch64 machined compiling, then the image build, then the qemu boot.

- [ ] **Step 2: Assess the outcome**

**Success:** the log shows `containerd ready=true name=containerd version=v2.0.9` then `BOOT TEST PASSED`. Record the total wall time + the aarch64 binary confirmation (`file` says ARM aarch64 static).

**If it boots but RuntimeReady never appears within the deadline under TCG** (containerd's Go runtime is heavy under emulation): diagnose from `target/boot-test/serial.log`:
```bash
grep -iE "machined starting|kernel modules|/boot|API|provision|containerd|cri|ttyAMA0|panic|oom" target/boot-test/serial.log | tail -40
```
- If machined + provisioning + API are up but containerd is just slow/OOM under TCG: per the spec §4 fallback, drop the aarch64 boot test's bar to **API-up + volumes-provisioned** (comment out / remove the RuntimeStatus loop in `scripts/boot-test-aarch64.sh`, leaving RuntimeReady to the x86 job + the Pi/manual path). Document this clearly as a deliberate TCG concession. Re-run to confirm the reduced bar passes.
- If `-m 512` OOMs under TCG (serial shows the oom-killer), bump the script's `-m` to `1024` and re-run.
- If the boot is silent (no machined output on serial): the console device is wrong — confirm `console=ttyAMA0` (not ttyS0) in both the qemu `-append` AND that machined's tracing reaches it.
- If a genuine code bug surfaces (a real aarch64/musl runtime failure in machined, not a TCG-speed/memory issue), STOP and report BLOCKED with the serial evidence — that's a fix-round, not a script tweak.

- [ ] **Step 3: Commit any script adjustment** (only if you changed the script — a timeout/memory bump or the documented RuntimeReady→API+volumes fallback)

```bash
git add scripts/boot-test-aarch64.sh
git commit -m "test(boot): tune aarch64 boot test for TCG (<timeout/memory/bar adjustment>)"
```

Report: did it reach RuntimeReady, or land on the API+volumes fallback? Total wall time (this sizes the CI `timeout-minutes` in Task 7). The `file` output proving the aarch64 binary is static. The serial log highlights.

---

### Task 7: CI job + finish

**Files:**
- Modify: `.github/workflows/ci.yml` (add `boot-test-aarch64` job)

This task is split across the two-PR finish (see "Finish" below) because the CI tool image must republish (with qemu-aarch64 + the aarch64 target) before the new job can pull it.

- [ ] **Step 1: Add the boot-test-aarch64 job** to `.github/workflows/ci.yml` — clone the existing `boot-test` job, drop the `--device /dev/kvm` (TCG, no cross-arch KVM), run `make boot-test-aarch64`, and size `timeout-minutes` from Task 6's measured wall time (round up generously — e.g. if local TCG took ~12 min, set 60). Distinct serial-log artifact name:

```yaml
  boot-test-aarch64:
    runs-on: ubuntu-latest
    needs: check
    timeout-minutes: 60
    permissions:
      contents: read
      packages: read
    container:
      image: ghcr.io/indyjonesnl/machined-ci:latest
      credentials:
        username: ${{ github.actor }}
        password: ${{ secrets.GITHUB_TOKEN }}
      # no --device /dev/kvm: cross-arch emulation is TCG, KVM doesn't apply.
    steps:
      - uses: actions/checkout@v4
      - uses: Swatinem/rust-cache@v2
      - name: cache imager artifacts
        uses: actions/cache@v4
        with:
          path: target/imager-cache
          key: imager-artifacts-${{ hashFiles('crates/imager/artifacts.toml') }}
      - name: boot test (aarch64)
        timeout-minutes: 50
        run: make boot-test-aarch64
      - name: upload serial log
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: boot-test-aarch64-serial-log
          path: target/boot-test/serial.log
          if-no-files-found: ignore
```

Validate YAML: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml')); print('ok')"`.

- [ ] **Step 2: Finish — two-PR rollout** (controller orchestrates; the CI image must republish first):

Follow superpowers:finishing-a-development-branch with this sequencing (mirrors the original GHCR rollout):
1. **PR-A** = Tasks 1-6 (imager arch, artifacts, Makefile, boot-test-aarch64.sh, **ci/Dockerfile**) WITHOUT the ci.yml job. Merge → `ci-image.yml` republishes `machined-ci:latest` with qemu-aarch64 + the aarch64 target. The existing x86 `boot-test` job still passes on the new image (qemu-system-x86_64 is still present). Confirm the ci-image run is green and the new image is published.
2. **PR-B** = the `boot-test-aarch64` job in ci.yml (Step 1). Open → the job pulls the republished image, builds + boots the aarch64 image under TCG, asserts the bar from Task 6. Merge once green.

Keep `main` green throughout (the image republishes before any job references qemu-aarch64).

---

## Verification (end-to-end)

1. `cargo test --workspace` green (the `VIRT_MODULES` rename + the aarch64 manifest test).
2. `make dist-aarch64` produces a static **ARM aarch64** machined binary (proven in the CI image, Task 6).
3. **The bar:** CI `boot-test-aarch64` boots the aarch64 image under `qemu-system-aarch64 -M virt` (TCG) and reaches the Task-6 bar — `RuntimeReady=true` if TCG allows, else API-up + volumes-provisioned (documented fallback). The existing x86 `boot-test` stays green.

## Known gaps / notes

- **TCG speed:** the aarch64 job is emulated (no cross-arch KVM) — minutes, not seconds. `timeout-minutes` is sized from the local run; if CI is slower/flakier than local, raise it or apply the API+volumes fallback.
- **RuntimeReady under TCG** may be the first thing to give — the spec sanctions dropping the aarch64 bar to API+volumes (RuntimeReady stays proven on x86 + the Pi path). Decided empirically in Task 6.
- **No arch-config table yet** — x86_64 and aarch64 share the kernel path + virtio module set, so a table would be premature. M7c-2 introduces it when the Pi config (linux-rpi kernel, MMC modules, firmware) actually diverges.
- **Pin freshness:** Alpine rolls `-rN`/patch within v3.21 and the mirror keeps only the latest; if a pinned apk 404s later, re-pin from the directory listing (Task 2 Step 1).
