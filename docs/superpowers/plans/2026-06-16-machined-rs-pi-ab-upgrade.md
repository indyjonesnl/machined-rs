# Pi-native A/B upgrade backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the Raspberry Pi 3A+ the same disk-persistent A/B OS upgrade x86 got in M9b-1, using the Pi firmware's `os_prefix` to select a self-contained slot dir (`/A`//`/B`) — proven on hardware.

**Architecture:** A `PiBootBackend` implements the existing `BootloaderBackend` trait: `stage_inactive` writes the new kernel+initramfs into the inactive slot dir (shared with `SdBootBackend` via a factored helper); `set_active` rewrites `config.txt`'s `os_prefix=` line (preserving all other lines, fsync-durable). The imager builds the Pi image with `os_prefix=A/`, scaffolds both slot dirs (dtb + per-slot `cmdline.txt` + `disable-bt` overlay), and bakes an `/etc/machined/bootloader` marker; machined picks the backend from the marker. The upgrade flow is unchanged.

**Tech Stack:** Rust (machined + imager), Raspberry Pi firmware `config.txt`/`os_prefix`, FAT32 (fatfs), the M9b-1 `BootloaderBackend` trait.

**Spec:** `docs/superpowers/specs/2026-06-16-machined-rs-pi-ab-upgrade-design.md`

---

## Orientation (read before starting)

- `crates/machined/src/bootloader.rs` — `Slot` (`other()`/`id()`/`dir()`), `parse_slot`, the `BootloaderBackend` trait, and `SdBootBackend` (the template). `SdBootBackend::stage_inactive` (L91-113) is the exact logic the Pi needs too — factor it into a shared free fn. `set_active` (L115-138) shows the rw-window + fsync-pointer + best-effort ro-reseal pattern to mirror.
- `crates/machined/src/main.rs` — `run_daemon` constructs `upgrade_backend: Arc<dyn bootloader::BootloaderBackend>` = `SdBootBackend::new("/boot", platform.clone(), &cmdline)` (~L413-419), then logs the booted slot. This becomes a marker-driven `match`.
- `crates/imager/src/rpi.rs` — `config_txt()`, `cmdline_txt()`, `stage_pi_firmware(rootfs, staging)` (stages blobs+dtb+config.txt+cmdline.txt to `staging` root). `PI3_DTB = "bcm2837-rpi-3-a-plus.dtb"`. The `linux-rpi` apk ships `rootfs/boot/overlays/disable-bt.dtbo`.
- `crates/imager/src/build.rs` — `stage_pi_firmware` is called (~L128-130) while `rootfs/boot` exists, BEFORE `prune_for_initramfs` deletes it and BEFORE `vmlinuz`/`initramfs.img` are written to `staging` root (~L138-143). The GPT `sdboot::assemble(&staging, cmdline)` runs later (~L177-185), AFTER the kernel/initramfs are in `staging` root — the Pi's "move kernel into /A" step goes at the same place. The `--emit-boot` block (~L186-204) copies the dtb from `staging.join(dtb)` — that path moves to `staging/A`.
- `crates/imager/src/initramfs.rs` — `build_initramfs(rootfs, machined, module_paths, kver, image_id)` writes `etc/machined/image-id` (L63). Add a `bootloader` param + `etc/machined/bootloader`. Callers: `build.rs:134` + 4 test callers in this file.
- `scripts/build-test-aarch64-rpi.sh` (run by `make build-image-aarch64-rpi`) — builds the Pi image and reads the FAT back. Extend its assertions for the A/B layout.
- `docs/raspberry-pi-3a-plus.md` — the hardware-verify doc; add the upgrade procedure.

**Hardware-only:** qemu can't run the VideoCore firmware, so the `os_prefix` boot-selection is verified by the operator on a real Pi 3A+. CI covers the layout + the backend's disk ops. The existing Pi CI jobs (`boot-test-aarch64-rpi` raspi3ap, `boot-test-aarch64-mbr`, `build-image-aarch64-rpi`) all boot via `--emit-boot`/external `-kernel` (NOT firmware), so the A/B FAT relocation must not break them — verify in Task 6.

---

## File Structure

- **Modify** `crates/machined/src/bootloader.rs` — factor `write_inactive_slot()`; add `PiBootBackend` + `rewrite_os_prefix()` + tests.
- **Modify** `crates/machined/src/main.rs` — `read_bootloader_marker()` + marker-driven backend selection.
- **Modify** `crates/imager/src/rpi.rs` — `os_prefix=A/` in config.txt; slot scaffolding (dtb + overlays + per-slot cmdline.txt into `/A`+`/B`); `move_kernel_to_slot_a()`.
- **Modify** `crates/imager/src/build.rs` — call the Pi move step; fix emit-boot dtb path; pass the bootloader marker.
- **Modify** `crates/imager/src/initramfs.rs` — `bootloader` param + write `etc/machined/bootloader`.
- **Modify** `scripts/build-test-aarch64-rpi.sh` — assert the A/B layout.
- **Modify** `docs/raspberry-pi-3a-plus.md` — hardware upgrade procedure.

---

## Task 1: `PiBootBackend` + shared slot-write helper (machined)

**Files:** Modify `crates/machined/src/bootloader.rs`.

- [ ] **Step 1: Factor the shared slot-write helper.** Add this free fn (above `SdBootBackend`), then change `SdBootBackend::stage_inactive` to use it.

```rust
/// Write kernel+initramfs into `slot`'s dir on the ESP, inside an rw remount
/// window (the ESP is mounted ro at runtime), fsync the dir, and re-seal ro.
/// Shared by SdBootBackend and PiBootBackend (their slot dirs are identical;
/// only the boot-pointer file differs). Returns the staging error (the ro
/// re-seal is best-effort) so the caller can still observe a failed write.
fn write_inactive_slot(
    esp: &Path,
    platform: &dyn Platform,
    slot: Slot,
    kernel: &Path,
    initrd: &Path,
) -> anyhow::Result<()> {
    let esp_s = esp.to_string_lossy().to_string();
    platform
        .remount_rw(&esp_s)
        .map_err(|e| anyhow::anyhow!("remount {esp_s} rw: {e}"))?;
    let res = (|| -> anyhow::Result<()> {
        let dir = esp.join(slot.dir());
        std::fs::create_dir_all(&dir)?;
        std::fs::copy(kernel, dir.join("vmlinuz"))?;
        std::fs::copy(initrd, dir.join("initramfs.img"))?;
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if let Err(e) = platform.remount_ro(&esp_s) {
        tracing::warn!("remount {esp_s} ro failed: {e}");
    }
    res
}
```

Replace `SdBootBackend::stage_inactive`'s body with:

```rust
    fn stage_inactive(&self, kernel: &Path, initrd: &Path) -> anyhow::Result<Slot> {
        let slot = self.current.other();
        write_inactive_slot(&self.esp, self.platform.as_ref(), slot, kernel, initrd)?;
        Ok(slot)
    }
```

- [ ] **Step 2: Run the existing tests — confirm no regression.** `cargo test -p machined bootloader` → the existing `sdboot_stages_inactive_and_flips_pointer` still passes (the refactor is behavior-preserving).

- [ ] **Step 3: Write `rewrite_os_prefix` with a failing test.** Add the pure helper + test:

```rust
/// Replace the value of config.txt's `os_prefix=` line with `<slot>/`, preserving
/// every other line and order. Errors if there is no os_prefix= line to flip
/// (the imager always writes one). The output always ends with a newline.
fn rewrite_os_prefix(config: &str, slot: Slot) -> anyhow::Result<String> {
    let mut found = false;
    let mut out = String::with_capacity(config.len() + 8);
    for line in config.lines() {
        if line.trim_start().starts_with("os_prefix=") {
            out.push_str(&format!("os_prefix={}/", slot.dir()));
            found = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    anyhow::ensure!(found, "config.txt has no os_prefix= line to flip");
    Ok(out)
}
```

Test (in the `tests` mod):

```rust
    #[test]
    fn rewrite_os_prefix_flips_only_that_line() {
        let cfg = "arm_64bit=1\nos_prefix=A/\nkernel=vmlinuz\ndtoverlay=disable-bt\n";
        let out = rewrite_os_prefix(cfg, Slot::B).unwrap();
        assert_eq!(
            out,
            "arm_64bit=1\nos_prefix=B/\nkernel=vmlinuz\ndtoverlay=disable-bt\n"
        );
        // every non-prefix line preserved
        assert!(out.contains("arm_64bit=1") && out.contains("kernel=vmlinuz") && out.contains("dtoverlay=disable-bt"));
        // no os_prefix line → error (don't silently brick the boot)
        assert!(rewrite_os_prefix("arm_64bit=1\nkernel=vmlinuz\n", Slot::B).is_err());
    }
```

- [ ] **Step 4: Run it** — `cargo test -p machined bootloader::tests::rewrite_os_prefix_flips_only_that_line` → PASS.

- [ ] **Step 5: Add `PiBootBackend`.** Mirror `SdBootBackend`, but `set_active` rewrites `config.txt`:

```rust
/// Raspberry Pi backend. The VideoCore firmware reads /boot/config.txt and an
/// `os_prefix=<slot>/` directive selects a self-contained slot dir (/A or /B).
/// stage_inactive writes the inactive slot's kernel+initramfs (the dtb,
/// cmdline.txt and overlays are scaffolded into the slot by the imager);
/// set_active flips config.txt's os_prefix line.
pub struct PiBootBackend {
    esp: std::path::PathBuf,
    platform: Arc<dyn Platform>,
    current: Slot,
}

impl PiBootBackend {
    pub fn new(
        esp: impl Into<std::path::PathBuf>,
        platform: Arc<dyn Platform>,
        cmdline: &str,
    ) -> Self {
        Self {
            esp: esp.into(),
            platform,
            current: parse_slot(cmdline),
        }
    }
}

impl BootloaderBackend for PiBootBackend {
    fn current_slot(&self) -> Slot {
        self.current
    }

    fn stage_inactive(&self, kernel: &Path, initrd: &Path) -> anyhow::Result<Slot> {
        let slot = self.current.other();
        write_inactive_slot(&self.esp, self.platform.as_ref(), slot, kernel, initrd)?;
        Ok(slot)
    }

    fn set_active(&self, slot: Slot) -> anyhow::Result<()> {
        let esp_s = self.esp.to_string_lossy().to_string();
        let conf = self.esp.join("config.txt");
        self.platform
            .remount_rw(&esp_s)
            .map_err(|e| anyhow::anyhow!("remount {esp_s} rw: {e}"))?;
        let res = (|| -> anyhow::Result<()> {
            let content = std::fs::read_to_string(&conf)
                .with_context(|| format!("read {}", conf.display()))?;
            let new = rewrite_os_prefix(&content, slot)?;
            std::fs::write(&conf, &new)?;
            // config.txt is THE Pi boot pointer: fsync the file + the dir so the
            // os_prefix flip survives a power cut before we re-seal the ESP ro.
            let _ = std::fs::File::open(&conf).and_then(|f| f.sync_all());
            if let Some(parent) = conf.parent() {
                let _ = std::fs::File::open(parent).and_then(|d| d.sync_all());
            }
            Ok(())
        })();
        if let Err(e) = self.platform.remount_ro(&esp_s) {
            tracing::warn!("remount {esp_s} ro failed: {e}");
        }
        res
    }
}
```

- [ ] **Step 6: Add a PiBootBackend test** (mirrors the sdboot one but checks config.txt):

```rust
    #[test]
    fn piboot_stages_inactive_and_flips_os_prefix() {
        use machined_platform::FakePlatform;
        let dir = tempfile::tempdir().unwrap();
        let esp = dir.path().join("boot");
        std::fs::create_dir_all(&esp).unwrap();
        std::fs::write(
            esp.join("config.txt"),
            "arm_64bit=1\nos_prefix=A/\nkernel=vmlinuz\ndtoverlay=disable-bt\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("vmlinuz"), b"K2").unwrap();
        std::fs::write(dir.path().join("initramfs.img"), b"I2").unwrap();

        let fake = std::sync::Arc::new(FakePlatform::new());
        let be = PiBootBackend::new(&esp, fake.clone(), "machined.slot=a");
        let esp_s = esp.to_string_lossy().to_string();
        let slot = be
            .stage_inactive(&dir.path().join("vmlinuz"), &dir.path().join("initramfs.img"))
            .unwrap();
        assert_eq!(slot, Slot::B);
        assert_eq!(std::fs::read(esp.join("B/vmlinuz")).unwrap(), b"K2");
        assert_eq!(std::fs::read(esp.join("B/initramfs.img")).unwrap(), b"I2");
        assert_eq!(fake.remounts(), vec![(esp_s.clone(), true), (esp_s.clone(), false)]);

        be.set_active(Slot::B).unwrap();
        // os_prefix flipped, all other lines intact.
        assert_eq!(
            std::fs::read_to_string(esp.join("config.txt")).unwrap(),
            "arm_64bit=1\nos_prefix=B/\nkernel=vmlinuz\ndtoverlay=disable-bt\n"
        );
        assert_eq!(
            fake.remounts(),
            vec![
                (esp_s.clone(), true),
                (esp_s.clone(), false),
                (esp_s.clone(), true),
                (esp_s.clone(), false),
            ]
        );
    }
```

- [ ] **Step 7:** Update the module doc comment (L1-3) — it currently says "a future PiBootBackend over autoboot.txt/tryboot"; change to "PiBootBackend over config.txt os_prefix".

- [ ] **Step 8: Run + lint.** `cargo test -p machined bootloader` (all pass), `cargo clippy -p machined --all-targets -- -D warnings` (clean), `cargo fmt -p machined -- --check`. NOTE: `PiBootBackend` is not constructed by non-test code until Task 2 — if clippy flags it dead, that's resolved IN Task 2 (same crate); if you must keep this task green standalone, a temporary module-level `#[allow(dead_code)]` is acceptable but Task 2 removes it. Prefer doing Task 2 right after so no allow is needed.

- [ ] **Step 9: Commit.**

```bash
git add crates/machined/src/bootloader.rs
git commit -m "feat(machined): PiBootBackend (os_prefix flip) + shared write_inactive_slot helper"
```

---

## Task 2: Bootloader marker — bake it, read it, select the backend

**Files:** Modify `crates/imager/src/initramfs.rs`, `crates/imager/src/build.rs`, `crates/machined/src/main.rs`.

- [ ] **Step 1: Add a `bootloader` param to `build_initramfs` + write the marker.** In `initramfs.rs`, change the signature and add the file write next to `image-id`:

```rust
pub fn build_initramfs(
    rootfs: &Path,
    machined: &Path,
    module_paths: &[String],
    kver: &str,
    image_id: &str,
    bootloader: &str,
) -> anyhow::Result<Vec<u8>> {
```

After the `image-id` line (L63) add:

```rust
    w.file("etc/machined/bootloader", 0o644, bootloader.as_bytes());
```

- [ ] **Step 2: Update the 4 test callers in `initramfs.rs`** — add a `"sdboot"` arg (the value doesn't matter for those tests; use `"sdboot"`). Add an assertion to `builds_gzip_cpio_with_init_console_and_modules_load` that `text.contains("etc/machined/bootloader")`.

- [ ] **Step 3: Update the `build.rs` caller** (~L134) to pass the marker derived from the arch:

```rust
    let bootloader_kind = if cfg.rpi_firmware { "pi" } else { "sdboot" };
    let initrd = initramfs::build_initramfs(&rootfs, o.machined, &mods, &kver, o.image_id, bootloader_kind)?;
```

- [ ] **Step 4: Run** `cargo test -p machined-imager initramfs` → PASS (signature + marker).

- [ ] **Step 5: Add `read_bootloader_marker` to `main.rs`** (testable — takes a path). Near `read_image_id` (~L226):

```rust
/// The bootloader backend baked into this initramfs by the imager
/// (/etc/machined/bootloader): "pi" → PiBootBackend, anything else → sdboot.
/// Absent/unreadable → "sdboot" (the default GPT/UEFI path).
fn read_bootloader_marker(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "sdboot".to_string())
}
```

Add a unit test in `main.rs`'s `tests` mod:

```rust
    #[test]
    fn bootloader_marker_defaults_to_sdboot_when_absent() {
        assert_eq!(read_bootloader_marker("/no/such/marker"), "sdboot");
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bootloader");
        std::fs::write(&p, "pi\n").unwrap();
        assert_eq!(read_bootloader_marker(p.to_str().unwrap()), "pi");
    }
```

(`tempfile` is already a machined dev-dependency.)

- [ ] **Step 6: Switch the backend construction** (~L413-419) to marker-driven:

```rust
    let upgrade_backend: Arc<dyn bootloader::BootloaderBackend> = {
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        match read_bootloader_marker("/etc/machined/bootloader").as_str() {
            "pi" => Arc::new(bootloader::PiBootBackend::new("/boot", platform.clone(), &cmdline)),
            _ => Arc::new(bootloader::SdBootBackend::new("/boot", platform.clone(), &cmdline)),
        }
    };
    info!("booted from A/B slot {}", upgrade_backend.current_slot().id());
```

(This makes `PiBootBackend` live, resolving any Task-1 dead-code concern — remove the temporary allow if you added one.)

- [ ] **Step 7: Build + test + lint.** `cargo test -p machined -p machined-imager` (all pass), `cargo clippy -p machined -p machined-imager --all-targets -- -D warnings` (clean — no dead_code allow on PiBootBackend), `cargo fmt --all -- --check`.

- [ ] **Step 8: Commit.**

```bash
git add crates/imager/src/initramfs.rs crates/imager/src/build.rs crates/machined/src/main.rs
git commit -m "feat: bake /etc/machined/bootloader marker + select PiBootBackend vs SdBootBackend"
```

---

## Task 3: Imager Pi A/B slot layout (`rpi.rs` + `build.rs`)

**Files:** Modify `crates/imager/src/rpi.rs`, `crates/imager/src/build.rs`.

- [ ] **Step 1: Add `os_prefix=A/` to config.txt + a per-slot cmdline helper + the overlay const.** In `rpi.rs`:

```rust
/// Overlays referenced by config.txt's dtoverlay= lines. os_prefix prepends to
/// overlay paths too, so each slot needs its own copy — most importantly
/// disable-bt, which puts the PL011 on the GPIO header (the serial console).
const PI3_OVERLAYS: &[&str] = &["disable-bt.dtbo"];
```

Change `config_txt()` to include `os_prefix=A/` (the firmware boots slot A by default; machined flips this on upgrade):

```rust
pub fn config_txt() -> &'static str {
    "arm_64bit=1\n\
     os_prefix=A/\n\
     kernel=vmlinuz\n\
     initramfs initramfs.img followkernel\n\
     gpu_mem=16\n\
     enable_uart=1\n\
     dtoverlay=disable-bt\n\
     device_tree=bcm2837-rpi-3-a-plus.dtb\n"
}
```

Replace `cmdline_txt()` with a per-slot variant (the slot token tells the running machined which slot booted):

```rust
/// cmdline.txt for a slot — the base console args plus the machined.slot token
/// (PiBootBackend reads it from /proc/cmdline via parse_slot). serial0 maps to
/// the PL011 on the header (disable-bt). machined is /init, so no root=.
pub fn cmdline_txt_for(slot: &str) -> String {
    format!("console=serial0,115200 console=tty1 machined.slot={slot}\n")
}
```

- [ ] **Step 2: Rework `stage_pi_firmware` to scaffold both slots.** It runs while `rootfs/boot` exists (so the dtb + overlays are available). Blobs + config.txt stay at the staging root; the dtb, overlays, and per-slot cmdline.txt go into BOTH `/A` and `/B`:

```rust
pub fn stage_pi_firmware(rootfs: &Path, staging: &Path) -> anyhow::Result<()> {
    let boot = rootfs.join("boot");
    // Firmware blobs + config.txt at the FAT root (read before os_prefix applies).
    for f in PI3_BLOBS {
        let src = boot.join(f);
        anyhow::ensure!(
            src.exists(),
            "Pi firmware blob {f} missing (raspberrypi-bootloader apks)"
        );
        std::fs::copy(&src, staging.join(f)).with_context(|| format!("stage {f}"))?;
    }
    std::fs::write(staging.join("config.txt"), config_txt()).context("write config.txt")?;

    // Each slot dir is self-contained: dtb + overlays + cmdline.txt (os_prefix
    // prepends to ALL of these). The kernel+initramfs are moved into /A later
    // (move_kernel_to_slot_a); /B's kernel is staged by the first upgrade.
    let dtb_src = boot.join(PI3_DTB);
    anyhow::ensure!(dtb_src.exists(), "Pi 3A+ DTB {PI3_DTB} missing (linux-rpi apk)");
    for (dir, id) in [("A", "a"), ("B", "b")] {
        let slot = staging.join(dir);
        let overlays = slot.join("overlays");
        std::fs::create_dir_all(&overlays)
            .with_context(|| format!("create slot dir {}", overlays.display()))?;
        std::fs::copy(&dtb_src, slot.join(PI3_DTB))
            .with_context(|| format!("stage {PI3_DTB} into {dir}"))?;
        for ovl in PI3_OVERLAYS {
            let src = boot.join("overlays").join(ovl);
            anyhow::ensure!(src.exists(), "Pi overlay {ovl} missing (linux-rpi apk)");
            std::fs::copy(&src, overlays.join(ovl))
                .with_context(|| format!("stage overlay {ovl} into {dir}"))?;
        }
        std::fs::write(slot.join("cmdline.txt"), cmdline_txt_for(id))
            .with_context(|| format!("write {dir}/cmdline.txt"))?;
    }
    Ok(())
}

/// Move the staged kernel+initramfs from the staging root into slot A. Called
/// AFTER the generic path writes staging/{vmlinuz,initramfs.img} (build.rs),
/// mirroring sdboot::assemble for the GPT arches.
pub fn move_kernel_to_slot_a(staging: &Path) -> anyhow::Result<()> {
    let a = staging.join("A");
    std::fs::create_dir_all(&a).with_context(|| format!("create {}", a.display()))?;
    for f in ["vmlinuz", "initramfs.img"] {
        std::fs::rename(staging.join(f), a.join(f))
            .with_context(|| format!("move {f} into slot A"))?;
    }
    Ok(())
}
```

- [ ] **Step 3: Wire `move_kernel_to_slot_a` into `build.rs`.** Right after the GPT `sdboot::assemble` block (~L185, before `image::write_image`), add the Pi equivalent:

```rust
    // Pi (MBR): move the kernel+initramfs into slot A; config.txt's os_prefix=A/
    // selects it. The slot dirs were scaffolded by stage_pi_firmware.
    if cfg.rpi_firmware {
        crate::rpi::move_kernel_to_slot_a(&staging)?;
    }
```

- [ ] **Step 4: Fix the emit-boot dtb path** in `build.rs` (~L197-201) — the dtb now lives in `staging/A`, not the root:

```rust
        if cfg.rpi_firmware {
            let dtb = crate::rpi::PI3_DTB;
            std::fs::copy(staging.join("A").join(dtb), dir.join(dtb))
                .with_context(|| format!("emit-boot DTB {dtb}"))?;
        }
```

- [ ] **Step 5: Update `rpi.rs` unit tests.** Replace `config_and_cmdline_have_the_pi3_essentials` and `stages_blobs_dtb_and_generated_configs` to assert the new layout:

```rust
    #[test]
    fn config_has_os_prefix_and_essentials() {
        let c = config_txt();
        assert!(c.contains("os_prefix=A/"), "{c}");
        assert!(c.contains("arm_64bit=1") && c.contains("kernel=vmlinuz"));
        assert!(c.contains("initramfs initramfs.img followkernel"));
        assert!(c.contains("dtoverlay=disable-bt") && c.contains("device_tree=bcm2837-rpi-3-a-plus.dtb"));
        assert!(cmdline_txt_for("a").contains("console=serial0,115200"));
        assert!(cmdline_txt_for("a").contains("machined.slot=a"));
        assert!(cmdline_txt_for("b").contains("machined.slot=b"));
    }

    #[test]
    fn scaffolds_both_slots_with_dtb_overlay_cmdline_and_moves_kernel() {
        let dir = tempfile::tempdir().unwrap();
        let (rootfs, staging) = (dir.path().join("rootfs"), dir.path().join("staging"));
        std::fs::create_dir_all(rootfs.join("boot/overlays")).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        for f in PI3_BLOBS {
            std::fs::write(rootfs.join("boot").join(f), f.as_bytes()).unwrap();
        }
        std::fs::write(rootfs.join("boot").join(PI3_DTB), b"dtb").unwrap();
        std::fs::write(rootfs.join("boot/overlays/disable-bt.dtbo"), b"ovl").unwrap();

        stage_pi_firmware(&rootfs, &staging).unwrap();
        // root: blobs + config.txt (os_prefix=A/), NOT cmdline.txt/dtb at root.
        for f in PI3_BLOBS {
            assert!(staging.join(f).exists());
        }
        assert!(std::fs::read_to_string(staging.join("config.txt")).unwrap().contains("os_prefix=A/"));
        // both slots scaffolded.
        for (d, id) in [("A", "a"), ("B", "b")] {
            assert_eq!(std::fs::read(staging.join(d).join(PI3_DTB)).unwrap(), b"dtb");
            assert_eq!(std::fs::read(staging.join(d).join("overlays/disable-bt.dtbo")).unwrap(), b"ovl");
            let cl = std::fs::read_to_string(staging.join(d).join("cmdline.txt")).unwrap();
            assert!(cl.contains(&format!("machined.slot={id}")), "{cl}");
        }
        // move_kernel_to_slot_a relocates the kernel/initramfs into /A.
        std::fs::write(staging.join("vmlinuz"), b"K").unwrap();
        std::fs::write(staging.join("initramfs.img"), b"I").unwrap();
        move_kernel_to_slot_a(&staging).unwrap();
        assert_eq!(std::fs::read(staging.join("A/vmlinuz")).unwrap(), b"K");
        assert_eq!(std::fs::read(staging.join("A/initramfs.img")).unwrap(), b"I");
        assert!(!staging.join("vmlinuz").exists());
        // /B has no kernel yet (staged by the first upgrade).
        assert!(!staging.join("B/vmlinuz").exists());
    }
```

Keep `missing_blob_is_an_error` (still valid). Add the overlays dir to its rootfs setup if needed (it bails on the missing blob first, so it's fine).

- [ ] **Step 6: Run** `cargo test -p machined-imager` → all pass (rpi + the happy-path build test — confirm the happy-path test, which builds x86_64, is unaffected; the Pi changes are gated on `cfg.rpi_firmware`).

- [ ] **Step 7: Lint + commit.** `cargo clippy -p machined-imager --all-targets -- -D warnings`, `cargo fmt -p machined-imager -- --check`.

```bash
git add crates/imager/src/rpi.rs crates/imager/src/build.rs
git commit -m "feat(imager): Pi A/B slot layout (os_prefix=A/, scaffold /A+/B with dtb/overlay/cmdline)"
```

---

## Task 4: Extend the FAT-readback layout test

**Files:** Modify `scripts/build-test-aarch64-rpi.sh`.

The script (no mount/losetup) has TWO embedded `python3` blocks: (1) an MBR check (KEEP it unchanged), and (2) a FAT32 **root-directory** walker that asserts a flat `want` list. The root-only walker is now wrong — `vmlinuz`/`initramfs.img`/`cmdline.txt`/the dtb moved into `/A`. **Replace the second python block** (the one that builds `want = [...]` and checks the root dir) with a recursive walker that descends into `/A`, `/B`, and their `overlays/`.

- [ ] **Step 1: Replace the second `python3 - "$IMG" <<'PY' … PY` block** with this (it reuses the same BPB/cluster math, adds `read_dir(first_cluster)` returning name→(kind, first_cluster), and asserts the A/B layout):

```python
import sys, struct
data = open(sys.argv[1], "rb").read()
base = 2048 * 512
bpb = data[base:base + 512]
bps = struct.unpack("<H", bpb[11:13])[0]
spc = bpb[13]
rsvd = struct.unpack("<H", bpb[14:16])[0]
nfats = bpb[16]
fatsz = struct.unpack("<I", bpb[36:40])[0]
root_clus = struct.unpack("<I", bpb[44:48])[0]
first_data = rsvd + nfats * fatsz
fat_off = base + rsvd * bps
def fat_entry(c):
    return struct.unpack("<I", data[fat_off + c * 4:fat_off + c * 4 + 4])[0] & 0x0FFFFFFF
def clus_bytes(c):
    sec = first_data + (c - 2) * spc
    off = base + sec * bps
    return data[off:off + spc * bps]
def chain_bytes(first):
    out, c, guard = b"", first, 0
    while 2 <= c < 0x0FFFFFF8 and guard < 100000:
        out += clus_bytes(c); c = fat_entry(c); guard += 1
    return out
def dec_lfn(e):
    return (e[1:11] + e[14:26] + e[28:32]).decode("utf-16-le", "replace").split("\x00")[0]
def read_dir(first):
    """name -> ('dir'|'file', first_cluster) for the directory at cluster `first`."""
    entries, lfn, out = chain_bytes(first), "", {}
    for i in range(0, len(entries), 32):
        e = entries[i:i + 32]
        if len(e) < 32 or e[0] == 0x00:
            break
        if e[0] == 0xE5:
            lfn = ""; continue
        if e[11] == 0x0F:
            lfn = dec_lfn(e) + lfn; continue
        short = e[0:8].rstrip().decode("latin1")
        ext = e[8:11].rstrip().decode("latin1")
        name = lfn or (short + ("." + ext if ext else "")); lfn = ""
        if name in (".", ".."):
            continue
        fc = (struct.unpack("<H", e[20:22])[0] << 16) | struct.unpack("<H", e[26:28])[0]
        out[name] = ("dir" if (e[11] & 0x10) else "file", fc)
    return out
def need(d, name, kind, where):
    assert name in d, f"missing {name} in {where} (have {sorted(d)})"
    assert d[name][0] == kind, f"{where}/{name} is {d[name][0]}, want {kind}"
    return d[name][1]
def text(first):  # whole clusters (trailing padding is harmless for substring checks)
    return chain_bytes(first).decode("latin1")

root = read_dir(root_clus)
# Root: config.txt + firmware blobs; the kernel/initramfs/dtb/cmdline are now in /A.
for f in ["config.txt", "bootcode.bin", "start.elf", "fixup.dat"]:
    need(root, f, "file", "root")
for f in ["vmlinuz", "initramfs.img", "cmdline.txt", "bcm2837-rpi-3-a-plus.dtb"]:
    assert f not in root, f"{f} must NOT be at FAT root (moved into /A)"
assert "os_prefix=A/" in text(root["config.txt"][1]), "config.txt missing os_prefix=A/"
a = read_dir(need(root, "A", "dir", "root"))
b = read_dir(need(root, "B", "dir", "root"))
# Slot A: full (kernel staged here at build).
for f in ["vmlinuz", "initramfs.img", "bcm2837-rpi-3-a-plus.dtb", "cmdline.txt"]:
    need(a, f, "file", "A")
assert "disable-bt.dtbo" in read_dir(need(a, "overlays", "dir", "A")), "A/overlays/disable-bt.dtbo missing"
assert "machined.slot=a" in text(a["cmdline.txt"][1]), "A/cmdline.txt missing machined.slot=a"
# Slot B: scaffolding only — NO kernel until the first upgrade.
for f in ["bcm2837-rpi-3-a-plus.dtb", "cmdline.txt"]:
    need(b, f, "file", "B")
assert "vmlinuz" not in b, "B/vmlinuz must be absent until the first upgrade"
assert "disable-bt.dtbo" in read_dir(need(b, "overlays", "dir", "B")), "B/overlays/disable-bt.dtbo missing"
assert "machined.slot=b" in text(b["cmdline.txt"][1]), "B/cmdline.txt missing machined.slot=b"
print("A/B os_prefix slot layout OK: root=config.txt+blobs, /A=full slot, /B=scaffold")
```

Also update the final `echo` line's message if it references "Pi boot files" to mention the A/B layout.

- [ ] **Step 2: Run it in the CI container** (the aarch64 cross-build lives there):

```bash
docker run --rm -v "$(pwd)":/work -w /work ghcr.io/indyjonesnl/machined-ci:latest \
  bash -c 'cd /work && make build-image-aarch64-rpi'
```

Expect the build + the new A/B layout assertions to pass. (Clean up root-owned `target/` scratch afterward via the container if needed.)

- [ ] **Step 3: Commit.**

```bash
git add scripts/build-test-aarch64-rpi.sh
git commit -m "test(rpi): assert the A/B os_prefix slot layout in the FAT readback"
```

---

## Task 5: Document the hardware upgrade procedure

**Files:** Modify `docs/raspberry-pi-3a-plus.md`.

- [ ] **Step 1: Add an "A/B upgrade" section.** Document: the layout (`config.txt os_prefix=A/`, self-contained `/A`//`/B` slots), and the operator procedure:
  1. Build a v2 bundle: `imager build --arch aarch64-rpi --image-id v2 … --emit-boot boot-v2` then `tar -czf bundle.tgz -C boot-v2 vmlinuz initramfs.img` + `sha256sum`.
  2. Serve it reachable from the Pi (HTTP) — e.g. `python3 -m http.server` on the workstation.
  3. On the booted Pi (v1, slot A): `machinectl --bundle … --endpoint https://<pi-ip>:50000 upgrade http://<host>:<port>/bundle.tgz <sha256>`.
  4. machined stages `/B/{vmlinuz,initramfs.img}`, flips `config.txt` to `os_prefix=B/`, and reboots.
  5. Over serial: the firmware boots `/B`; confirm `machinectl version` → `image_id=v2` and `booted from A/B slot b` in the log.
  6. **Manual rollback:** mount the FAT and re-flip `os_prefix=A/` (or `machinectl upgrade` back), reboot → v1.

  Also note the watch-out: if the serial goes dark after an upgrade, check `/B/overlays/disable-bt.dtbo` is present (os_prefix applies to overlays). Update the "What you should see" section to mention `booted from A/B slot a` and the `os_prefix` boot.

- [ ] **Step 2: Commit.**

```bash
git add docs/raspberry-pi-3a-plus.md
git commit -m "docs(rpi): A/B upgrade procedure + os_prefix layout + rollback"
```

---

## Task 6: Final verification (no regressions)

**Files:** none (verification only).

- [ ] **Step 1: Full local gate.** `make pre-commit` → fmt + clippy -D warnings + all tests green.

- [ ] **Step 2: The existing Pi CI jobs must still pass** (they boot via `--emit-boot`/external `-kernel`, NOT firmware, so the A/B FAT relocation shouldn't break them — but the dtb now comes from `staging/A`). In the CI container, run all three Pi jobs:

```bash
docker run --rm -v "$(pwd)":/work -w /work ghcr.io/indyjonesnl/machined-ci:latest bash -c '
  cd /work
  make build-image-aarch64-rpi   # FAT readback incl. new A/B assertions
  make boot-test-aarch64-rpi     # raspi3ap external-kernel boot → node up (marker, machined.slot defaults to A)
  make boot-test-aarch64-mbr     # MBR /boot mount on -M virt (unaffected by /A//B)'
```

All three must pass. The raspi3ap + mbr tests boot the `/A` kernel via `--emit-boot` (the dtb emit now reads `staging/A`), so confirm they still reach their markers.

- [ ] **Step 3: x86 unaffected.** The x86 image now bakes `/etc/machined/bootloader=sdboot` and machined reads it → `SdBootBackend` (same as before). Run the x86 cold-reboot upgrade test once to confirm no regression:

```bash
docker run --rm -v "$(pwd)":/work -w /work ghcr.io/indyjonesnl/machined-ci:latest bash -c '
  apt-get update >/dev/null && apt-get install -y ovmf python3 >/dev/null
  cd /work && make boot-test'   # expect: BOOT TEST PASSED (disk A/B upgrade ... COLD reboot ...)
```

- [ ] **Step 4: Note the milestone.** Update the README status table: add a row for the Pi A/B upgrade (CI: layout + backend; hardware-verified end-to-end). Commit.

---

## Self-review notes

- **Spec coverage:** PiBootBackend (T1), marker + selection (T2), imager A/B layout + os_prefix + overlays (T3), FAT-readback test (T4), hardware doc (T5), no-regression verify (T6). All spec sections covered.
- **The `disable-bt` watch-out** (serial dies without it) → staged into both slots in T3, asserted in T3 unit test + T4 FAT readback.
- **config.txt preserve-other-lines** → `rewrite_os_prefix` + its unit test (T1).
- **Shared `write_inactive_slot`** removes the SdBootBackend/PiBootBackend duplication (the M9b-1 code-review note) — T1 refactors SdBootBackend onto it (behavior-preserving; its existing test guards that).
- **Type consistency:** `bootloader: &str` param threads build_initramfs (T2) ↔ the marker file ↔ `read_bootloader_marker` (T2); `PI3_OVERLAYS`/`move_kernel_to_slot_a`/`cmdline_txt_for` defined in T3 and used in T3's build.rs wiring + tests.
- **No qemu end-to-end** (hardware-only) is by design; T6 guards that the A/B relocation doesn't regress the existing external-kernel Pi jobs.
