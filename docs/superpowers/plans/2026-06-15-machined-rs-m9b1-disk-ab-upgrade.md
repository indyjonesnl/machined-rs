# M9b-1 — Disk-persistent A/B upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** machined persists an OS upgrade to disk in A/B slots and boots the new image from disk via systemd-boot, so an upgrade survives a cold reboot — proven end-to-end by the x86_64 boot test (boot from disk → upgrade → cold reboot → node is v2 with STATE/PKI intact).

**Architecture:** The imager stages systemd-boot + an A/B slot layout onto the existing EFI/ESP FAT partition; `loader.conf default` is the boot pointer. A platform-agnostic `BootloaderBackend` trait (impl `SdBootBackend`) lets machined write the new kernel+initramfs into the inactive slot and flip the pointer; the upgrade then does a **cold reboot** (not kexec). The x86 boot test switches from qemu `-kernel` to **OVMF + boot-from-disk**.

**Tech Stack:** Rust (imager + machined), systemd-boot (EFI), OVMF (qemu UEFI firmware), FAT32 ESP, `fatfs`/`gpt` crates (imager), `nix` mount (platform).

**Spec:** `docs/superpowers/specs/2026-06-15-machined-rs-m9b1-disk-ab-upgrade-design.md`

---

## Orientation (read before starting)

Key existing code this plan touches:

- `crates/imager/src/build.rs` — artifact-kind dispatch (`match a.kind.as_str()` ~L86) and FAT staging assembly (`staging.join("vmlinuz")`, config, pki, `--emit-boot` ~L136-178). The staging tree is copied verbatim into the FAT by `image::write_image`.
- `crates/imager/src/image.rs` — `write_image_gpt` formats the EFI FAT32 and `copy_tree`s `staging` into it. **No change needed** — it copies whatever subtree we assemble (so `/EFI/BOOT/BOOTX64.EFI`, `/loader/...`, `/A/...` just work if they're in `staging`).
- `crates/imager/src/manifest.rs` + `crates/imager/artifacts.toml` — pinned artifacts; `kind` dispatch. Add an `sd-boot-efi` kind + pin the binary.
- `crates/imager/src/arch.rs` — `ArchConfig { scheme: PartScheme }`. sd-boot applies to `PartScheme::Gpt` arches (x86_64, aarch64).
- `crates/machined/src/main.rs` — the `FinalAction` loop (~L427-504). `Upgrade` currently calls `upgrade::prepare` → `FinalAction::Kexec`. We switch it to disk-persist → `FinalAction::Reboot`.
- `crates/machined/src/upgrade.rs` — `prepare` does download→verify→extract→`kexec_load`. We reuse the first three and replace `kexec_load` with slot-stage + pointer-flip.
- `crates/machined/src/imageboot.rs` — `mount_boot` mounts the ESP at `/boot` `MS_RDONLY` and the chosen `VolumeInfo.device` is the ESP block device.
- `crates/platform/src/lib.rs` + `linux.rs` + `fake.rs` — `Platform` trait. `mount`, `unmount`, `reboot`, `MS_RDONLY` exist. We add `remount_rw`/`remount_ro`.
- `scripts/boot-test-x86_64.sh` — boots via `-kernel`/`-initrd`; does the M9a kexec upgrade assertion (L165-192). We switch to OVMF disk boot + cold-reboot assertion.
- `ci/Dockerfile` — the CI tool image; add `ovmf`.

**Empirical reality:** the sd-boot binary source/sha, the exact `loader.conf`/entry syntax that boots the Alpine `vmlinuz` under OVMF, and the OVMF qemu flags are discovered + verified by the boot test (Task 2's spike), exactly like the Pi dtb work. Do NOT invent shas — compute them, and let the boot test be the arbiter.

---

## File Structure

- **Create** `crates/imager/src/sdboot.rs` — assemble the sd-boot ESP layout (BOOTX64.EFI placement, `loader.conf`, `loader/entries/{a,b}.conf`, move kernel+initramfs under `/A`) into the staging tree. One responsibility: "turn a staging dir into a systemd-boot A/B ESP." Unit-tested against a temp dir.
- **Create** `crates/machined/src/bootloader.rs` — `Slot`, `BootloaderBackend` trait, `current_slot_from_cmdline`, and `SdBootBackend` (writes slot files + rewrites `loader.conf default` under a given ESP mount path). Unit-tested against a temp dir + `FakePlatform`.
- **Modify** `crates/imager/src/{build.rs,manifest.rs,arch.rs}`, `crates/imager/artifacts.toml` — pin + stage sd-boot for GPT arches.
- **Modify** `crates/machined/src/{upgrade.rs,main.rs}` — disk-persist upgrade flow + cold reboot.
- **Modify** `crates/platform/src/{lib.rs,linux.rs,fake.rs}` — `remount_rw`/`remount_ro`.
- **Modify** `crates/resources` (UpgradePhase) — add a `Staged` phase.
- **Modify** `scripts/boot-test-x86_64.sh`, `ci/Dockerfile`.

---

## Task 1: Pin systemd-boot + add the `sd-boot-efi` artifact kind

**Files:**
- Modify: `crates/imager/artifacts.toml`
- Modify: `crates/imager/src/build.rs` (kind dispatch ~L86-105)
- Modify: `crates/imager/src/manifest.rs` (doc comment on `kind` + a test)

- [ ] **Step 1: Obtain + pin the systemd-boot EFI binary (x86_64 first).**

systemd-boot ships as a standalone PE binary inside the `systemd-boot-efi` (Debian/Ubuntu) or `systemd-boot` (Fedora) package. Fetch a pinned `.deb`, extract `usr/lib/systemd/boot/efi/systemd-bootx64.efi`, host it on the project's GHCR releases (same pattern as the custom kernel), and pin URL+sha256. Concretely, run locally to get the binary + sha:

```bash
# Example sourcing (pin whatever stable version you settle on):
curl -fsSLO http://deb.debian.org/debian/pool/main/s/systemd/systemd-boot-efi_252.36-1~deb12u1_amd64.deb
dpkg-deb --fsys-tarfile systemd-boot-efi_*_amd64.deb | tar -xO ./usr/lib/systemd/boot/efi/systemd-bootx64.efi > systemd-bootx64.efi
sha256sum systemd-bootx64.efi   # <- this is the value to pin
```

Upload `systemd-bootx64.efi` (and the aarch64 `systemd-bootaa64.efi` from the `arm64` .deb) to a GHCR release (e.g. tag `sdboot-252`), then add to `[artifact].x86_64` in `artifacts.toml`:

```toml
  { name = "systemd-boot", url = "https://github.com/indyjonesnl/machined-rs/releases/download/sdboot-252/systemd-bootx64.efi", sha256 = "<computed>", kind = "sd-boot-efi" },
```

and the equivalent `systemd-bootaa64.efi` under `[artifact].aarch64`.

- [ ] **Step 2: Write the failing manifest test.**

In `crates/imager/src/manifest.rs` `real_artifacts_manifest_parses` (or a new test), add:

```rust
    // M9b-1: systemd-boot EFI binary pinned for the GPT (UEFI) arches.
    assert!(x86.iter().any(|a| a.name == "systemd-boot" && a.kind == "sd-boot-efi"));
    assert!(arm.iter().any(|a| a.name == "systemd-boot" && a.kind == "sd-boot-efi"));
```

- [ ] **Step 3: Run it — expect FAIL** (`cargo test -p machined-imager manifest`) until the `artifacts.toml` entries from Step 1 are added. Add them; re-run → PASS.

- [ ] **Step 4: Handle the new kind in `build.rs`.**

In the `match a.kind.as_str()` block (~L86), add an arm that stages the EFI binary to `staging/EFI/BOOT/BOOTX64.EFI` (x86) / `BOOTAA64.EFI` (aarch64). The removable-media default path the firmware loads is `/EFI/BOOT/BOOT<ARCH>.EFI`. Decide the target name from `o.arch`:

```rust
            "sd-boot-efi" => {
                // Firmware's removable-media fallback path. x86_64 → BOOTX64.EFI,
                // aarch64 → BOOTAA64.EFI; both are the same systemd-boot binary
                // for that arch, just at the path UEFI auto-loads.
                let efi_name = match o.arch {
                    "aarch64" => "BOOTAA64.EFI",
                    _ => "BOOTX64.EFI",
                };
                let dst = staging.join("EFI/BOOT").join(efi_name);
                std::fs::create_dir_all(dst.parent().unwrap())
                    .with_context(|| format!("create {}", dst.parent().unwrap().display()))?;
                std::fs::copy(&path, &dst)
                    .with_context(|| format!("stage sd-boot {}", dst.display()))?;
            }
```

(`path` is the fetched artifact file; mirror the existing `boot-binary` arm's use of `path`.)

- [ ] **Step 5: Commit.**

```bash
git add crates/imager/artifacts.toml crates/imager/src/build.rs crates/imager/src/manifest.rs
git commit -m "feat(imager): pin systemd-boot EFI binary + sd-boot-efi artifact kind"
```

---

## Task 2: Assemble the A/B sd-boot ESP layout (imager `sdboot.rs`) + boot spike

**Files:**
- Create: `crates/imager/src/sdboot.rs`
- Modify: `crates/imager/src/main.rs` (`mod sdboot;`)
- Modify: `crates/imager/src/build.rs` (call it for GPT arches; move vmlinuz/initramfs under `/A`)

- [ ] **Step 1: Write `sdboot.rs` with a unit-tested assembler.**

```rust
//! Assemble a systemd-boot A/B layout inside the imager's FAT staging tree.
//! The ESP gets: /loader/loader.conf (default=a), /loader/entries/{a,b}.conf,
//! and slot A populated at /A/{vmlinuz,initramfs.img}. Slot B is created by the
//! first on-device upgrade (machined), not here. The systemd-boot binary itself
//! is staged separately (build.rs, the sd-boot-efi artifact kind).

use anyhow::Context as _;
use std::path::Path;

/// loader.conf: pick slot A by default, no menu timeout (headless).
fn loader_conf() -> &'static str {
    "default a\ntimeout 0\n"
}

/// A type-1 boot entry for slot `slot` ("a"/"b"). `cmdline` is the kernel
/// command line WITHOUT the slot token; we append `machined.slot=<slot>` so the
/// running machined knows which slot it booted (bootloader.rs reads /proc/cmdline).
fn entry_conf(slot: &str, cmdline: &str) -> String {
    format!(
        "title machined ({slot})\n\
         linux /{up}/vmlinuz\n\
         initrd /{up}/initramfs.img\n\
         options {cmdline} machined.slot={slot}\n",
        up = slot.to_uppercase()
    )
}

/// Lay out systemd-boot A/B inside `staging`, moving the already-staged
/// `staging/vmlinuz` + `staging/initramfs.img` into `staging/A/`. `cmdline` is
/// the base kernel cmdline (e.g. "console=ttyS0").
pub fn assemble(staging: &Path, cmdline: &str) -> anyhow::Result<()> {
    // /A holds slot A's kernel+initramfs (moved from the staging root).
    let slot_a = staging.join("A");
    std::fs::create_dir_all(&slot_a).with_context(|| format!("create {}", slot_a.display()))?;
    for f in ["vmlinuz", "initramfs.img"] {
        std::fs::rename(staging.join(f), slot_a.join(f))
            .with_context(|| format!("move {f} into slot A"))?;
    }
    // /loader/loader.conf + entries.
    let entries = staging.join("loader/entries");
    std::fs::create_dir_all(&entries).with_context(|| format!("create {}", entries.display()))?;
    std::fs::write(staging.join("loader/loader.conf"), loader_conf())
        .context("write loader.conf")?;
    std::fs::write(entries.join("a.conf"), entry_conf("a", cmdline)).context("write a.conf")?;
    std::fs::write(entries.join("b.conf"), entry_conf("b", cmdline)).context("write b.conf")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_moves_kernel_and_writes_loader() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        std::fs::write(s.join("vmlinuz"), b"k").unwrap();
        std::fs::write(s.join("initramfs.img"), b"i").unwrap();

        assemble(s, "console=ttyS0").unwrap();

        // kernel+initramfs moved into /A; gone from root.
        assert_eq!(std::fs::read(s.join("A/vmlinuz")).unwrap(), b"k");
        assert_eq!(std::fs::read(s.join("A/initramfs.img")).unwrap(), b"i");
        assert!(!s.join("vmlinuz").exists());
        // loader.conf points at slot a; entries carry the slot token.
        assert_eq!(std::fs::read_to_string(s.join("loader/loader.conf")).unwrap(), "default a\ntimeout 0\n");
        let a = std::fs::read_to_string(s.join("loader/entries/a.conf")).unwrap();
        assert!(a.contains("linux /A/vmlinuz"), "{a}");
        assert!(a.contains("machined.slot=a"), "{a}");
        let b = std::fs::read_to_string(s.join("loader/entries/b.conf")).unwrap();
        assert!(b.contains("linux /B/vmlinuz") && b.contains("machined.slot=b"), "{b}");
    }
}
```

- [ ] **Step 2: Run the unit test** — `cargo test -p machined-imager sdboot` → PASS.

- [ ] **Step 3: Wire into `build.rs` for GPT arches.**

In `build.rs`, after the kernel+initramfs+config+pki are staged and BEFORE `image::write_image` (i.e. just before L163), call the assembler for sd-boot arches. sd-boot applies to `PartScheme::Gpt` (the UEFI arches); the Pi (`Mbr`) keeps its flat layout. The base cmdline matches today's boot tests (`console=ttyS0` for x86 / `console=ttyAMA0` for aarch64) — pick it from the arch:

```rust
    // M9b-1: GPT (UEFI) arches boot from disk via systemd-boot in an A/B layout.
    // The Pi (MBR) keeps the flat firmware-loaded layout.
    if cfg.scheme == crate::arch::PartScheme::Gpt {
        let cmdline = match o.arch {
            "aarch64" => "console=ttyAMA0",
            _ => "console=ttyS0",
        };
        crate::sdboot::assemble(&staging, cmdline)?;
    }
```

Add `mod sdboot;` to `crates/imager/src/main.rs`.

- [ ] **Step 4: BOOT SPIKE — prove sd-boot+OVMF actually boots the image from disk.**

This is the de-risking step (the sd-boot/OVMF integration is the real unknown). In the CI container, build an x86_64 image (now with the sd-boot layout) and boot it under OVMF **from the disk** (no `-kernel`). The CI image needs `ovmf` — install it ad-hoc for the spike (`apt-get update && apt-get install -y ovmf`) before Task 7 bakes it in.

```bash
docker run --rm -v "$(pwd)":/work -w /work ghcr.io/indyjonesnl/machined-ci:latest bash -c '
  set -e; cd /work; export CARGO_TARGET_DIR=target
  apt-get update >/dev/null && apt-get install -y ovmf >/dev/null
  cargo build --release -p machined-imager >/dev/null
  CARGO_TARGET_AARCH64... # (x86 build) — use: make dist-x86_64 equivalent:
  cargo build --release --target x86_64-unknown-linux-musl -p machined >/dev/null
  W=target/spike; rm -rf $W; mkdir -p $W
  target/release/machined-imager gen-pki --out $W/pki
  target/release/machined-imager build --arch x86_64 \
    --machined target/x86_64-unknown-linux-musl/release/machined \
    --config examples/node-ci.yaml --pki-dir $W/pki --image-id v1 \
    --out $W/img.img --cache target/imager-cache
  cp /usr/share/OVMF/OVMF_CODE.fd $W/code.fd; cp /usr/share/OVMF/OVMF_VARS.fd $W/vars.fd
  timeout 120 qemu-system-x86_64 -m 512 -machine q35 \
    -drive if=pflash,format=raw,unit=0,readonly=on,file=$W/code.fd \
    -drive if=pflash,format=raw,unit=1,file=$W/vars.fd \
    -drive file=$W/img.img,if=virtio,format=raw \
    -display none -serial file:$W/serial.log -no-reboot || true
  echo "=== serial ==="; tail -40 $W/serial.log
'
```

Expected: the serial shows systemd-boot loading, then the kernel booting, then `machined starting (pid 1)`. If sd-boot can't find/boot the entry, iterate on `loader.conf`/entry syntax and the OVMF flags here (boot-test-as-arbiter). **Do not proceed to Task 7 until this spike boots machined from disk.** Capture the working OVMF invocation for Task 7.

- [ ] **Step 5: Commit** (once the spike boots).

```bash
git add crates/imager/src/sdboot.rs crates/imager/src/main.rs crates/imager/src/build.rs
git commit -m "feat(imager): assemble systemd-boot A/B ESP layout for GPT arches"
```

---

## Task 3: `Slot` + `BootloaderBackend` trait + current-slot detection (machined)

**Files:**
- Create: `crates/machined/src/bootloader.rs`
- Modify: `crates/machined/src/main.rs` (`mod bootloader;`)

- [ ] **Step 1: Write the trait, `Slot`, and cmdline parsing with tests.**

```rust
//! Boot-slot selection for A/B disk upgrades. A BootloaderBackend abstracts the
//! per-platform bootloader: SdBootBackend (UEFI/systemd-boot) here; a future
//! PiBootBackend over autoboot.txt/tryboot. The upgrade flow is backend-agnostic.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    pub fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
    /// Lowercase id used in loader.conf / cmdline ("a"/"b").
    pub fn id(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }
    /// Uppercase ESP subdir ("A"/"B").
    pub fn dir(self) -> &'static str {
        match self {
            Slot::A => "A",
            Slot::B => "B",
        }
    }
}

/// Parse `machined.slot=a|b` out of a kernel command line. Absent/garbage → A
/// (the imager always writes the token; default A keeps a hand-booted node sane).
pub fn parse_slot(cmdline: &str) -> Slot {
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("machined.slot=") {
            if v == "b" {
                return Slot::B;
            }
            return Slot::A;
        }
    }
    Slot::A
}

/// Backend over the on-disk bootloader. `esp` is the mounted ESP root (/boot).
pub trait BootloaderBackend {
    /// Which slot the running kernel booted from.
    fn current_slot(&self) -> Slot;
    /// Write kernel+initramfs into the inactive slot dir on the ESP; return it.
    fn stage_inactive(&self, kernel: &Path, initrd: &Path) -> anyhow::Result<Slot>;
    /// Flip the boot pointer (loader.conf default) to `slot`.
    fn set_active(&self, slot: Slot) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slot_reads_token_or_defaults_a() {
        assert_eq!(parse_slot("console=ttyS0 machined.slot=b"), Slot::B);
        assert_eq!(parse_slot("machined.slot=a console=ttyS0"), Slot::A);
        assert_eq!(parse_slot("console=ttyS0"), Slot::A); // absent → A
        assert_eq!(parse_slot("machined.slot=x"), Slot::A); // garbage → A
    }

    #[test]
    fn slot_other_and_ids() {
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
        assert_eq!((Slot::A.id(), Slot::A.dir()), ("a", "A"));
        assert_eq!((Slot::B.id(), Slot::B.dir()), ("b", "B"));
    }
}
```

Add `mod bootloader;` to `main.rs`.

- [ ] **Step 2: Run** `cargo test -p machined bootloader` → PASS.

- [ ] **Step 3: Commit.**

```bash
git add crates/machined/src/bootloader.rs crates/machined/src/main.rs
git commit -m "feat(machined): BootloaderBackend trait + Slot + machined.slot cmdline parse"
```

---

## Task 4: Platform `remount_rw`/`remount_ro` + `SdBootBackend`

**Files:**
- Modify: `crates/platform/src/lib.rs` (trait + `MS_REMOUNT` const)
- Modify: `crates/platform/src/linux.rs` (real impl)
- Modify: `crates/platform/src/fake.rs` (fake records remounts)
- Modify: `crates/machined/src/bootloader.rs` (`SdBootBackend`)

- [ ] **Step 1: Add `remount_rw`/`remount_ro` to the `Platform` trait.**

In `crates/platform/src/lib.rs`, near `MS_RDONLY` add `pub const MS_REMOUNT: u64 = 0x20;`, and in `trait Platform` add (with no default, both impls update):

```rust
    /// Remount an already-mounted target read-write (MS_REMOUNT|original flags).
    fn remount_rw(&self, target: &str) -> Result<()>;
    /// Remount an already-mounted target read-only.
    fn remount_ro(&self, target: &str) -> Result<()>;
```

- [ ] **Step 2: Implement in `linux.rs`.** Mirror the existing `mount` impl (uses `nix::mount::mount`):

```rust
    fn remount_rw(&self, target: &str) -> Result<()> {
        nix::mount::mount(
            None::<&str>, target, None::<&str>,
            nix::mount::MsFlags::MS_REMOUNT,
            None::<&str>,
        )
        .map_err(|e| PlatformError::Mount { target: target.into(), message: format!("remount rw: {e}") })
    }
    fn remount_ro(&self, target: &str) -> Result<()> {
        nix::mount::mount(
            None::<&str>, target, None::<&str>,
            nix::mount::MsFlags::MS_REMOUNT | nix::mount::MsFlags::MS_RDONLY,
            None::<&str>,
        )
        .map_err(|e| PlatformError::Mount { target: target.into(), message: format!("remount ro: {e}") })
    }
```

- [ ] **Step 3: Implement in `fake.rs`.** Record calls so the backend is testable. Add a `remounts: Mutex<Vec<(String,bool)>>` field (true=rw) to the recorded struct and:

```rust
    fn remount_rw(&self, target: &str) -> Result<()> {
        self.recorded.lock().unwrap().remounts.push((target.into(), true));
        Ok(())
    }
    fn remount_ro(&self, target: &str) -> Result<()> {
        self.recorded.lock().unwrap().remounts.push((target.into(), false));
        Ok(())
    }
```

(Find the `recorded` struct in `fake.rs` and add the `remounts` vec to it + its initializer.)

- [ ] **Step 4: Run** `cargo build -p machined-platform` → compiles. (No behavior test needed for the fake recorder beyond compile; the backend test below exercises it.)

- [ ] **Step 5: Write `SdBootBackend` in `bootloader.rs` with a failing test.**

```rust
use machined_platform::Platform;
use std::sync::Arc;

/// systemd-boot backend. `esp` is the mounted ESP root (machined mounts the EFI
/// partition at /boot). Writes slot dirs + rewrites loader.conf's `default`.
pub struct SdBootBackend {
    esp: std::path::PathBuf,
    platform: Arc<dyn Platform>,
    current: Slot,
}

impl SdBootBackend {
    /// `esp` = the ESP mount point (/boot). `cmdline` = /proc/cmdline contents.
    pub fn new(esp: impl Into<std::path::PathBuf>, platform: Arc<dyn Platform>, cmdline: &str) -> Self {
        Self { esp: esp.into(), platform, current: parse_slot(cmdline) }
    }
}

impl BootloaderBackend for SdBootBackend {
    fn current_slot(&self) -> Slot {
        self.current
    }

    fn stage_inactive(&self, kernel: &Path, initrd: &Path) -> anyhow::Result<Slot> {
        let slot = self.current.other();
        let esp = self.esp.to_string_lossy().to_string();
        // The ESP is mounted ro at runtime; remount rw for the staging window.
        self.platform.remount_rw(&esp).map_err(|e| anyhow::anyhow!("remount {esp} rw: {e}"))?;
        let res = (|| -> anyhow::Result<()> {
            let dir = self.esp.join(slot.dir());
            std::fs::create_dir_all(&dir)?;
            std::fs::copy(kernel, dir.join("vmlinuz"))?;
            std::fs::copy(initrd, dir.join("initramfs.img"))?;
            // fsync the dir so the writes hit disk before we (later) reboot.
            if let Ok(d) = std::fs::File::open(&dir) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        // Always remount ro, even on error.
        let _ = self.platform.remount_ro(&esp);
        res?;
        Ok(slot)
    }

    fn set_active(&self, slot: Slot) -> anyhow::Result<()> {
        let esp = self.esp.to_string_lossy().to_string();
        let conf = self.esp.join("loader/loader.conf");
        self.platform.remount_rw(&esp).map_err(|e| anyhow::anyhow!("remount {esp} rw: {e}"))?;
        let res = std::fs::write(&conf, format!("default {}\ntimeout 0\n", slot.id()))
            .map_err(anyhow::Error::from);
        let _ = self.platform.remount_ro(&esp);
        res.with_context(|| format!("write {}", conf.display()))
    }
}
```

Test (uses `FakePlatform`, which lets the real `std::fs` writes land in a temp dir — the fake's remount is a no-op recorder so the fs ops still execute):

```rust
    #[test]
    fn sdboot_stages_inactive_and_flips_pointer() {
        use machined_platform::FakePlatform;
        let dir = tempfile::tempdir().unwrap();
        let esp = dir.path().join("boot");
        std::fs::create_dir_all(esp.join("loader")).unwrap();
        std::fs::write(esp.join("loader/loader.conf"), "default a\ntimeout 0\n").unwrap();
        // a v2 bundle on disk:
        std::fs::write(dir.path().join("vmlinuz"), b"K2").unwrap();
        std::fs::write(dir.path().join("initramfs.img"), b"I2").unwrap();

        let be = SdBootBackend::new(&esp, std::sync::Arc::new(FakePlatform::new()), "machined.slot=a");
        // running A → inactive is B
        let slot = be.stage_inactive(&dir.path().join("vmlinuz"), &dir.path().join("initramfs.img")).unwrap();
        assert_eq!(slot, Slot::B);
        assert_eq!(std::fs::read(esp.join("B/vmlinuz")).unwrap(), b"K2");
        assert_eq!(std::fs::read(esp.join("B/initramfs.img")).unwrap(), b"I2");

        be.set_active(Slot::B).unwrap();
        assert_eq!(std::fs::read_to_string(esp.join("loader/loader.conf")).unwrap(), "default b\ntimeout 0\n");
    }
```

- [ ] **Step 6: Run** `cargo test -p machined bootloader` → PASS.

- [ ] **Step 7: Commit.**

```bash
git add crates/platform/src crates/machined/src/bootloader.rs
git commit -m "feat: SdBootBackend (stage inactive slot + flip loader.conf) + platform remount"
```

---

## Task 5: Disk-persist upgrade flow (`upgrade.rs`) + `UpgradePhase::Staged`

**Files:**
- Modify: `crates/resources/...` (the `UpgradePhase` enum — add `Staged`)
- Modify: `crates/machined/src/upgrade.rs`

- [ ] **Step 1: Add `Staged` to `UpgradePhase`.**

Find the `UpgradePhase` enum (it's a closed enum in `crates/resources`; `grep -rn "enum UpgradePhase" crates/`). Add a `Staged` variant alongside `Downloading/Verifying/Loaded/Failed`, and update every exhaustive `match` the compiler flags (the API field-mapping has no wildcard arm by design — follow the compile errors). Run `cargo build -p machined-resources -p machined-apiserver` and fix each non-exhaustive match.

- [ ] **Step 2: Add `prepare_disk` to `upgrade.rs`** (reuses the existing download/verify/extract helpers — keep `http_get`, `sha256_hex`, `extract_bundle`):

```rust
use crate::bootloader::BootloaderBackend;

/// Download + verify + extract, then stage the new kernel+initramfs into the
/// inactive A/B slot and flip the boot pointer. On success the node is ready to
/// COLD reboot into the new slot. On ANY failure: UpgradeStatus=Failed, node
/// stays up (graceful-abort, as M9a). `backend` is the platform bootloader.
pub async fn prepare_disk(
    state: &State,
    backend: &dyn BootloaderBackend,
    url: &str,
    sha256: &str,
) -> anyhow::Result<()> {
    publish(state, UpgradePhase::Downloading, url);
    let url_owned = url.to_string();
    let bytes = match tokio::task::spawn_blocking(move || http_get(&url_owned)).await? {
        Ok(b) => b,
        Err(e) => { publish(state, UpgradePhase::Failed, &e.to_string()); return Err(e); }
    };

    publish(state, UpgradePhase::Verifying, "");
    let got = sha256_hex(&bytes);
    if !got.eq_ignore_ascii_case(sha256) {
        let msg = format!("sha256 mismatch: got {got}, want {sha256}");
        publish(state, UpgradePhase::Failed, &msg);
        anyhow::bail!(msg);
    }

    let dir = Path::new(STAGE_DIR);
    let (kernel, initrd) = match extract_bundle(&bytes, dir) {
        Ok(v) => v,
        Err(e) => { publish(state, UpgradePhase::Failed, &e.to_string()); return Err(e); }
    };

    // Stage into the inactive slot on the ESP, then flip the pointer (commit).
    let slot = match backend.stage_inactive(&kernel, &initrd) {
        Ok(s) => s,
        Err(e) => { publish(state, UpgradePhase::Failed, &e.to_string()); return Err(e); }
    };
    if let Err(e) = backend.set_active(slot) {
        publish(state, UpgradePhase::Failed, &e.to_string());
        return Err(e);
    }
    info!("upgrade staged to slot {} and committed; cold reboot to apply", slot.id());
    publish(state, UpgradePhase::Staged, slot.id());
    Ok(())
}
```

- [ ] **Step 3: Add a test** (FakePlatform-backed SdBootBackend + a real http server is overkill; test the staging path directly with a stub backend):

```rust
    struct StubBackend { staged: std::sync::Mutex<Option<Slot>>, active: std::sync::Mutex<Option<Slot>> }
    impl crate::bootloader::BootloaderBackend for StubBackend {
        fn current_slot(&self) -> Slot { Slot::A }
        fn stage_inactive(&self, _k: &Path, _i: &Path) -> anyhow::Result<Slot> {
            *self.staged.lock().unwrap() = Some(Slot::B); Ok(Slot::B)
        }
        fn set_active(&self, s: Slot) -> anyhow::Result<()> { *self.active.lock().unwrap() = Some(s); Ok(()) }
    }
```

Add a `#[tokio::test]` that serves a known bundle over a localhost `tiny_http`/`std::net` one-shot server (or reuse the M9a test bundle helper + a `httptest`-style stub) and asserts `prepare_disk` returns Ok, the stub recorded `staged=B` and `active=B`, and `UpgradeStatus` ends `Staged`. If wiring a real HTTP server is heavy, factor the post-download half into a `stage_and_commit(state, backend, bytes, sha) ` helper and test THAT directly with in-memory bytes (preferred — no socket in a unit test).

- [ ] **Step 4: Run** `cargo test -p machined upgrade` → PASS.

- [ ] **Step 5: Commit.**

```bash
git add crates/resources crates/machined/src/upgrade.rs crates/machined/src/apiserver* 2>/dev/null; git add -A
git commit -m "feat(machined): disk-persist upgrade — stage inactive slot + flip pointer (UpgradePhase::Staged)"
```

---

## Task 6: Wire the Upgrade action to disk-persist + cold reboot (`main.rs`)

**Files:**
- Modify: `crates/machined/src/main.rs`

- [ ] **Step 1: Build the backend + switch the Upgrade arm.**

In `run_daemon`, construct the backend once the ESP is known. The ESP is mounted at `/boot`; read `/proc/cmdline` for the slot. Add near the other `state_for_*` clones (~L410):

```rust
    let upgrade_backend: Arc<dyn bootloader::BootloaderBackend> = {
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        Arc::new(bootloader::SdBootBackend::new("/boot", platform.clone(), &cmdline))
    };
    let backend_for_upgrade = upgrade_backend.clone();
```

Change the `Upgrade` arm in the `final_action` loop (~L436) from `upgrade::prepare(...) → Kexec` to:

```rust
            Some(NodeAction::Upgrade { url, sha256 }) => {
                match upgrade::prepare_disk(
                    &state_for_upgrade, backend_for_upgrade.as_ref(), &url, &sha256,
                ).await {
                    Ok(()) => break FinalAction::Reboot, // cold reboot into the new slot
                    Err(e) => { error!("upgrade aborted (node stays up): {e}"); continue; }
                }
            }
```

- [ ] **Step 2: Remove the now-unused `FinalAction::Kexec` arm + variant** (M9b-1 supersedes the in-memory kexec upgrade with disk-persist + cold reboot). Delete the `Kexec` enum variant and its match arm (~L497-503). If you prefer to keep kexec capability, leave the variant but it's no longer produced — `cargo build` will warn about unused; prefer removing it for cleanliness. Note: `platform.kexec_load`/`reboot_kexec` stay on the trait (still used by tests / future).

- [ ] **Step 3: Build + run machined tests** — `cargo build -p machined && cargo test -p machined` → PASS (the existing FakePlatform daemon tests should be unaffected; `SdBootBackend` is only constructed on the real path).

- [ ] **Step 4: Commit.**

```bash
git add crates/machined/src/main.rs
git commit -m "feat(machined): Upgrade action stages to disk + cold reboots (supersedes in-memory kexec)"
```

---

## Task 7: x86 boot test → OVMF disk boot + cold-reboot-to-v2

**Files:**
- Modify: `scripts/boot-test-x86_64.sh`
- Modify: `ci/Dockerfile` (add `ovmf`)

- [ ] **Step 1: Add `ovmf` to the CI image.** In `ci/Dockerfile`'s apt install list add `ovmf \` and a sanity check line (e.g. `&& test -f /usr/share/OVMF/OVMF_CODE.fd \`). Add a comment explaining it provides x86 UEFI firmware for the M9b-1 disk boot. **This is a two-phase rollout** (Dockerfile change republishes `:latest`, then the boot job uses it) — same as the python3 change.

- [ ] **Step 2: Switch the qemu invocation to OVMF disk boot.** Replace the `-kernel`/`-initrd`/`-append` block (L55-61) with the working invocation captured in Task 2's spike. OVMF needs per-run writable VARS:

```bash
cp /usr/share/OVMF/OVMF_CODE.fd "$WORK/ovmf_code.fd"
cp /usr/share/OVMF/OVMF_VARS.fd "$WORK/ovmf_vars.fd"
# shellcheck disable=SC2086
qemu-system-x86_64 $KVM_FLAG -m 512 -machine q35 \
  -drive if=pflash,format=raw,unit=0,readonly=on,file="$WORK/ovmf_code.fd" \
  -drive if=pflash,format=raw,unit=1,file="$WORK/ovmf_vars.fd" \
  -drive file="$IMG",if=virtio,format=raw \
  -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${PORT}-:50000" \
  -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$SERIAL" &
```

(No `-kernel`/`-initrd` — systemd-boot on the ESP boots the active slot. The v1 image is built WITHOUT `--emit-boot` now, since qemu no longer needs the external pair; keep `--emit-boot "$WORK/boot-v2"` for the v2 bundle, which still tars vmlinuz+initramfs.)

- [ ] **Step 3: Replace the M9a kexec assertion (L165-192) with cold-reboot-to-v2.** The upgrade RPC + version polling stays, but the proof is now a cold reboot. machined does the cold reboot itself (`FinalAction::Reboot`); qemu must NOT have `-no-reboot`, so the guest reboots and OVMF re-reads the disk → slot B. Keep the existing post-upgrade poll loop (it already waits for `image_id=v2` + STATE Provisioned); update the success message:

```bash
echo "BOOT TEST PASSED (disk A/B upgrade v1->v2 survived a COLD reboot, STATE persisted)"
```

Also assert the boot was truly from disk: before the upgrade, the serial should show systemd-boot (grep the serial for a sd-boot marker, e.g. `systemd-boot` or `Loading.*vmlinuz`), and after the upgrade the new boot likewise. Add a soft check:

```bash
grep -qiE "systemd-boot|Linux-Boot|/A/vmlinuz" "$SERIAL" || echo "WARN: no sd-boot marker in serial (boot path?)"
```

- [ ] **Step 4: Verify end-to-end in the CI container** (with ad-hoc ovmf, since the image rebuild lands in CI):

```bash
docker run --rm -v "$(pwd)":/work -w /work ghcr.io/indyjonesnl/machined-ci:latest bash -c '
  apt-get update >/dev/null && apt-get install -y ovmf python3 >/dev/null
  cd /work && make boot-test'   # expect: BOOT TEST PASSED (disk A/B upgrade ... COLD reboot ...)
```

Iterate until green (the cold-reboot-to-v2 is the headline proof). Watch for: OVMF VARS needing to persist the boot entry across the reboot (sd-boot writes `LoaderEntryDefault` to an EFI var — but we also flip `loader.conf default`, which is file-based and authoritative, so a fresh VARS each boot is fine; confirm the second boot picks slot B from `loader.conf`, not a stale EFI var).

- [ ] **Step 5: Commit.**

```bash
git add scripts/boot-test-x86_64.sh ci/Dockerfile
git commit -m "test(boot-x86): OVMF disk boot + prove disk A/B upgrade survives a cold reboot"
```

---

## Task 8: Final verification + docs

**Files:**
- Modify: `docs/raspberry-pi-3a-plus.md` or `README.md` (status note — optional, light)

- [ ] **Step 1: Full local gate.** `make pre-commit` (fmt + clippy -D warnings + tests) → all green.

- [ ] **Step 2: Full container boot test** (Task 7 Step 4) once more from a clean `target/boot-test` → `BOOT TEST PASSED`.

- [ ] **Step 3: Push + watch CI.** Because `ci/Dockerfile` changed, the `ci-image.yml` workflow republishes `:latest` first; then the `boot-test` job (on the new image) must go green. Confirm all jobs pass (`gh run view <id>`), especially `boot-test` (now disk boot) and that aarch64/rpi/mbr jobs still pass (their images gained the sd-boot layout but still boot via their existing paths — the aarch64-virt boot still uses `-kernel` and is unaffected; verify it didn't regress).

- [ ] **Step 4: Note the milestone.** Update the README status table line for upgrade (M9b-1 done: disk A/B + cold-reboot-to-v2; M9b-2 = health-gated rollback next). Commit.

```bash
git add README.md && git commit -m "docs: README — M9b-1 disk A/B upgrade (cold-reboot survival) done"
```

---

## Self-review notes (gaps the implementer must watch)

- **aarch64 regression risk:** the imager now assembles the sd-boot layout for `aarch64` too (it's GPT), moving its `vmlinuz`/`initramfs.img` under `/A`. But `scripts/boot-test-aarch64.sh` boots via `--emit-boot` + external `-kernel` (it reads `$WORK/boot/vmlinuz`, which still exists because `--emit-boot` writes the pair to a SEPARATE dir, not the ESP). Confirm `--emit-boot` still emits the pair (build.rs L164-173 writes from `kernel_bytes`/`initrd`, not from staging, so it's unaffected). The aarch64 image's ESP layout changes but its test doesn't read the ESP kernel — verify the aarch64 boot test still passes in Task 8 Step 3.
- **`aarch64-mbr` + `aarch64-rpi`:** `PartScheme::Mbr` → sd-boot assembler NOT called → unchanged. Good.
- **EFI VARS persistence:** the file-based `loader.conf default` is authoritative for slot choice; do not rely on EFI-var persistence (fresh OVMF VARS per boot is fine). Verified in Task 7 Step 4.
- **Partial slot on crash:** if machined crashes mid-`stage_inactive`, the active slot + pointer are untouched → node still boots the current slot; the partial inactive slot is overwritten by the next upgrade. No commit happens before both files are written + the pointer flips.
- **DRY:** `prepare_disk` reuses `http_get`/`sha256_hex`/`extract_bundle` from M9a — do not duplicate them.
