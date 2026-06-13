# M7b-2 — Runtime on Metal (containerd + runc) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship containerd + runc as pinned static binaries on the FAT `/boot/bin`, supervise containerd with a functional `version = 3` config, and have the QEMU boot test assert `RuntimeStatus.ready=true` — proving the CRI plugin loads on a real boot.

**Architecture:** New imager artifact kinds (`boot-tarball`, `boot-binary`) stage binaries into `staging/bin/` (→ FAT `/boot/bin`), separate from the initramfs rootfs. machined mounts cgroup2, puts `/boot/bin` on PATH, and generates a containerd 2.x CRI config; `node-ci.yaml` enables the runtime. The existing M4a `RuntimeHealthController` already probes CRI and publishes `RuntimeStatus`; the boot test asserts it.

**Tech Stack:** Rust, flate2+tar (already imager deps), containerd 2.0.9 static + runc 1.4.3 (pinned), cgroup v2, QEMU/KVM boot test.

**Decisive research findings (do not re-derive):**
- **`RuntimeReady` is hardcoded `true`** in containerd `internal/cri/server/status.go` — it reflects *only* that the CRI runtime plugin loaded and answers `Status`. It does NOT require runc present, cgroups mounted, or `subtree_control` delegated. So the M7b-2 bar (`ready=true`) needs only: containerd running + CRI plugin loaded (config parses, plugin not disabled) + socket reachable. **Cgroup subtree-delegation and a PID1 leaf-cgroup move are NOT needed for this milestone** (only for actually launching a pod — a later milestone). We still mount cgroup2 and ship runc + point the config at it, because the goal is a functional runtime, not a hollow one.
- containerd 2.x config is **`version = 3`**; the CRI plugin key is **`io.containerd.cri.v1.runtime`** (was `io.containerd.grpc.v1.cri` in 1.x/v2 schema). CRI is **enabled by default** in the upstream static tarball (Docker's repackaged build disabled it; the upstream one does not).
- Use the **`-static`** asset: `containerd-static-2.0.9-linux-amd64.tar.gz` (fully static, no libc), containing `bin/{containerd,containerd-shim-runc-v2,ctr,containerd-stress}`. runc: `runc.amd64` v1.4.3 (static-pie). **Verify both sha256 by downloading at pin time — do not trust any pre-supplied hash.**
- Alpine `linux-virt` 6.12.93 has cgroup v2 + memory/pids/io/cpu controllers built-in (`CONFIG_MEMCG/CGROUP_PIDS/BLK_CGROUP/CFS_BANDWIDTH=y`). overlay/veth/bridge are `=m` (not needed for RuntimeReady; needed later for pods).

**Verified code facts:**
- `crates/imager/src/manifest.rs:14-21` `Artifact { name, url, sha256, kind: String }` (free-form kind). `crates/imager/src/build.rs:63-72` fetch loop with `match a.kind.as_str() { "apk" => apk::extract_apk(&path, &rootfs)?, ... }`; staging built at `build.rs:90-110` (`staging = scratch/staging`; writes vmlinuz/initramfs.img/config.yaml/pki/; then `image::write_image(o.out, o.size, &staging)`).
- Containment guard pattern (`crates/imager/src/apk.rs:49-54`): reject entry unless every `path.components()` is `Component::Normal(_)`.
- `crates/config/src/runtime_svc.rs:24-28` `containerd_config_toml(rt)` returns the minimal `version = 2` stub; `containerd_service(rt)` (`:9-21`) execs `[rt.binary, "--config", rt.config_path]`, `restart: Always`. `RuntimeSection` (`crates/config/src/types.rs:142-163`): `{ disabled, binary, socket, config_path }`, defaults `/usr/bin/containerd`, `/run/containerd/containerd.sock`, `/etc/containerd/config.toml`.
- `crates/machined/src/main.rs:213-218` `default_path_if_unset(...)` returns `"/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"`.
- `crates/platform/src/lib.rs:45-60` `essential_mounts()`; `MountSpec { source, target, fstype, flags: u64, data }`; consts `MS_RDONLY/NOSUID/NODEV`. `crates/platform/src/fake.rs` records `mounts: Vec<MountSpec>` (add a `mounts()` accessor, mirroring `modules_loaded()`).
- `RuntimeStatus { ready, name, version }` published in namespace `"runtime"` (`crates/controllers/src/runtime/`); API prints `ready=… name=… version=…` (`crates/apiserver/src/mapping.rs:107-111`).
- `examples/node-ci.yaml` ends with `runtime:\n    disabled: true`.
- `scripts/boot-test-x86_64.sh`: `ctl()` = `timeout 15 "$CTL" --bundle … --endpoint https://127.0.0.1:${PORT} "$@"`; VolumeStatus assertion loop at the end.

---

### Task 1: Imager boot-partition staging (boot-tarball + boot-binary)

**Files:**
- Create: `crates/imager/src/boot.rs`
- Modify: `crates/imager/src/main.rs` (add `mod boot;`)

- [ ] **Step 1: Write the failing tests** (in `crates/imager/src/boot.rs`)

```rust
//! Staging external binaries onto the FAT boot partition (/boot/bin): the
//! containerd static tarball and the runc binary. Distinct from apk extraction
//! (which targets the initramfs rootfs) — these land on disk, not in RAM.

use anyhow::Context;
use std::path::{Component, Path};

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn tar_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            for (name, data) in entries {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h.clone(), name, *data).unwrap();
            }
            b.finish().unwrap();
        }
        buf
    }

    #[test]
    fn boot_tarball_stages_bin_entries_executable() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("containerd.tar.gz");
        std::fs::write(
            &tgz,
            gzip(&tar_with(&[
                ("bin/containerd", b"\x7fELF-c"),
                ("bin/containerd-shim-runc-v2", b"\x7fELF-s"),
                ("bin/ctr", b"\x7fELF-x"),
                // non-bin entries are ignored
                ("LICENSE", b"license"),
            ])),
        )
        .unwrap();
        let staging_bin = dir.path().join("staging").join("bin");
        extract_boot_tarball(&tgz, &staging_bin).unwrap();

        assert_eq!(std::fs::read(staging_bin.join("containerd")).unwrap(), b"\x7fELF-c");
        assert_eq!(std::fs::read(staging_bin.join("containerd-shim-runc-v2")).unwrap(), b"\x7fELF-s");
        assert_eq!(std::fs::read(staging_bin.join("ctr")).unwrap(), b"\x7fELF-x");
        assert!(!staging_bin.join("LICENSE").exists(), "non-bin entries skipped");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(staging_bin.join("containerd")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "must be executable");
        }
    }

    #[test]
    fn boot_tarball_rejects_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let tgz = dir.path().join("evil.tar.gz");
        // Build a tar carrying a hostile name via raw header bytes (append_data
        // sanitizes), mirroring the apk escape test.
        let mut raw = Vec::new();
        {
            let mut b = tar::Builder::new(&mut raw);
            let mut h = tar::Header::new_gnu();
            let name = b"bin/../../etc/evil";
            h.as_old_mut().name[..name.len()].copy_from_slice(name);
            h.set_size(3);
            h.set_mode(0o755);
            h.set_cksum();
            b.append(&h, &b"bad"[..]).unwrap();
            b.finish().unwrap();
        }
        std::fs::write(&tgz, gzip(&raw)).unwrap();
        let staging_bin = dir.path().join("staging").join("bin");
        let err = extract_boot_tarball(&tgz, &staging_bin).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
    }

    #[test]
    fn boot_binary_copies_with_rename_executable() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("runc.amd64");
        std::fs::write(&src, b"\x7fELF-runc").unwrap();
        let staging_bin = dir.path().join("staging").join("bin");
        copy_boot_binary(&src, &staging_bin, "runc").unwrap();
        assert_eq!(std::fs::read(staging_bin.join("runc")).unwrap(), b"\x7fELF-runc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(staging_bin.join("runc")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined-imager boot`
Expected: FAIL — `cannot find function extract_boot_tarball` / `copy_boot_binary`.

- [ ] **Step 3: Implement** (above the tests in `boot.rs`)

```rust
/// Guard: every component of an archive entry path must be Normal (no `..`,
/// no absolute, no prefix) — same posture as apk extraction.
fn guard_contained(path: &Path) -> anyhow::Result<()> {
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        anyhow::bail!("boot-tarball entry escapes staging: {}", path.display());
    }
    Ok(())
}

fn set_exec(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod 0755 {}", path.display()))?;
    }
    Ok(())
}

/// Extract `bin/*` files from a single-stream `.tar.gz` into `staging_bin`,
/// flattened (bin/containerd -> staging_bin/containerd), mode 0755. Non-`bin/`
/// entries are ignored. The official containerd static tarball is exactly this
/// shape (bin/containerd, bin/containerd-shim-runc-v2, bin/ctr, …).
///
/// # Errors
/// Fails on I/O errors or an entry whose path escapes containment (`..`/absolute).
pub fn extract_boot_tarball(tgz: &Path, staging_bin: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(tgz).with_context(|| format!("opening {}", tgz.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    std::fs::create_dir_all(staging_bin).with_context(|| format!("create {}", staging_bin.display()))?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        guard_contained(&path)?;
        let mut comps = path.components();
        if comps.next().map(|c| c.as_os_str()) != Some(std::ffi::OsStr::new("bin")) {
            continue; // only bin/* is staged
        }
        let rest: std::path::PathBuf = comps.collect();
        if rest.as_os_str().is_empty() || !entry.header().entry_type().is_file() {
            continue; // the bin/ dir entry itself, or a nested non-file
        }
        let target = staging_bin.join(&rest);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf)?;
        std::fs::write(&target, &buf).with_context(|| format!("write {}", target.display()))?;
        set_exec(&target)?;
    }
    Ok(())
}

/// Copy a single static binary into `staging_bin` under `name`, mode 0755
/// (e.g. runc.amd64 -> staging_bin/runc).
///
/// # Errors
/// Fails on I/O errors.
pub fn copy_boot_binary(src: &Path, staging_bin: &Path, name: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(staging_bin).with_context(|| format!("create {}", staging_bin.display()))?;
    let target = staging_bin.join(name);
    std::fs::copy(src, &target).with_context(|| format!("copy {} -> {}", src.display(), target.display()))?;
    set_exec(&target)?;
    Ok(())
}
```

Add `mod boot;` to `crates/imager/src/main.rs` (next to the other `mod` lines). Item-level `#[allow(dead_code)] // wired in Task 2` on `extract_boot_tarball` + `copy_boot_binary` if clippy flags them before Task 2 wires them (binary crate). Remove in Task 2.

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p machined-imager boot && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`
Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/imager/src/boot.rs crates/imager/src/main.rs
git commit -m "feat(imager): boot.rs — stage containerd tarball + runc binary to /boot/bin"
```

---

### Task 2: Wire boot kinds into the build pipeline

**Files:**
- Modify: `crates/imager/src/manifest.rs` (optional `rename` field)
- Modify: `crates/imager/src/build.rs` (staging created before the loop; dispatch boot kinds)

- [ ] **Step 1: Add `rename` to `Artifact`** (`manifest.rs`)

```rust
#[derive(Clone, Debug, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub url: String,
    pub sha256: String,
    /// "apk" → initramfs rootfs; "boot-tarball" → /boot/bin (bin/* from a
    /// single .tar.gz); "boot-binary" → /boot/bin/<rename|name>.
    pub kind: String,
    /// For "boot-binary": the filename to stage as (e.g. runc). Ignored otherwise.
    #[serde(default)]
    pub rename: Option<String>,
}
```

(Adding a `#[serde(default)]` field is backward-compatible — existing apk entries omit it.)

- [ ] **Step 2: Failing test** — extend the build pipeline test (`build.rs` tests) to include a synthetic boot-tarball + boot-binary and assert they land in the image's FAT `/bin`. In the existing `happy_path_builds_image_with_all_boot_files` test (or a focused new test `build_stages_boot_binaries`), add to the in-test manifest two artifacts and assert the FAT read-back contains `bin/containerd` and `bin/runc`. Sketch of the assertion (adapt to the test's existing FAT read-back helper):

```rust
    // ... after building the image with a manifest that also pins:
    //   { name="containerd", kind="boot-tarball", url=<file served by the map fetcher>, sha256=… }
    //   { name="runc", kind="boot-binary", rename="runc", url=…, sha256=… }
    // assert the FAT /bin subdir holds the staged binaries:
    let bin = fat_root.open_dir("bin").expect("bin dir on FAT");
    let names: Vec<String> = bin.iter().map(|e| e.unwrap().file_name())
        .filter(|n| n != "." && n != "..").collect();
    assert!(names.contains(&"containerd".to_string()), "{names:?}");
    assert!(names.contains(&"runc".to_string()), "{names:?}");
```

Build the synthetic boot-tarball with the Task-1 test helpers (a gz of a tar with `bin/containerd`) and the runc "binary" as raw bytes; register both in the map-backed test fetcher with their real sha256 (compute with sha2, as the existing build tests do). Reuse the existing test's fetcher/manifest scaffolding — do not duplicate the whole test; extend it or factor a shared `#[cfg(test)]` helper.

- [ ] **Step 3: Run, verify it fails**

Run: `cargo test -p machined-imager build_`
Expected: FAIL — boot kinds not dispatched; no `bin/` on the FAT image.

- [ ] **Step 4: Implement the dispatch** (`build.rs`)

Move staging creation **before** the fetch loop, and create `staging/bin`:

```rust
    let scratch = tempfile::tempdir().context("creating scratch dir")?;
    let rootfs = scratch.path().join("rootfs");
    let staging = scratch.path().join("staging");
    let staging_bin = staging.join("bin");
    std::fs::create_dir_all(&staging).with_context(|| format!("creating staging dir {}", staging.display()))?;
    for a in arts {
        println!("fetching {} ({})", a.name, a.url);
        let path = crate::fetch::fetch_verified(fetcher, &a.url, &a.sha256, o.cache)?;
        match a.kind.as_str() {
            "apk" => apk::extract_apk(&path, &rootfs)?,
            "boot-tarball" => crate::boot::extract_boot_tarball(&path, &staging_bin)?,
            "boot-binary" => {
                let name = a.rename.clone().unwrap_or_else(|| a.name.clone());
                crate::boot::copy_boot_binary(&path, &staging_bin, &name)?;
            }
            k => anyhow::bail!("unknown artifact kind {k} for {}", a.name),
        }
    }
```

Then DELETE the later `let staging = scratch.path().join("staging"); std::fs::create_dir_all(&staging)…` lines (staging now exists from above) and keep the vmlinuz/initramfs/config/pki writes targeting the same `staging`. The `staging/bin` (possibly empty if no boot artifacts) rides along into `image::write_image`. Remove the Task-1 `#[allow(dead_code)]` from boot.rs (now wired).

- [ ] **Step 5: Run, verify pass + gates**

Run: `cargo test -p machined-imager && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`
Expected: all imager tests pass, including the new boot-staging assertion.

- [ ] **Step 6: Commit**

```bash
git add crates/imager/src/manifest.rs crates/imager/src/build.rs crates/imager/src/boot.rs
git commit -m "feat(imager): dispatch boot-tarball/boot-binary kinds into /boot/bin"
```

---

### Task 3: Functional containerd config (version = 3)

**Files:**
- Modify: `crates/config/src/runtime_svc.rs` (`containerd_config_toml`)
- Modify: `crates/config/Cargo.toml` (add `toml` dev-dep for the parse test, if absent)

- [ ] **Step 1: Failing test** (in `runtime_svc.rs` tests, or add a test module)

```rust
#[cfg(test)]
mod config_tests {
    use super::*;
    use crate::types::RuntimeSection;

    #[test]
    fn containerd_config_is_v3_cri_with_runc_cgroupfs() {
        let rt = RuntimeSection { socket: "/run/containerd/containerd.sock".into(), ..Default::default() };
        let toml_str = containerd_config_toml(&rt);
        // Parses as valid TOML.
        let parsed: toml::Value = toml::from_str(&toml_str).expect("valid TOML");
        assert_eq!(parsed.get("version").and_then(|v| v.as_integer()), Some(3));
        // The 2.x CRI runtime plugin key is present.
        assert!(toml_str.contains("io.containerd.cri.v1.runtime"), "{toml_str}");
        // runc runtime via the v2 shim, cgroupfs driver, explicit runc path.
        assert!(toml_str.contains("runtime_type = \"io.containerd.runc.v2\""), "{toml_str}");
        assert!(toml_str.contains("SystemdCgroup = false"), "{toml_str}");
        assert!(toml_str.contains("BinaryName = \"/boot/bin/runc\""), "{toml_str}");
        // root/state dirs set.
        assert!(toml_str.contains("root = \"/var/lib/containerd\""), "{toml_str}");
        assert!(toml_str.contains("state = \"/run/containerd\""), "{toml_str}");
        // socket threaded through.
        assert!(toml_str.contains("address = \"/run/containerd/containerd.sock\""), "{toml_str}");
    }
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined-config containerd_config`
Expected: FAIL — current output is `version = 2` with the old plugin key (and possibly `toml` dep missing → add it to `[dev-dependencies]` in `crates/config/Cargo.toml`: `toml = "0.8"`, then the assertions fail on content).

- [ ] **Step 3: Implement** — replace `containerd_config_toml` body:

```rust
/// Generate a functional containerd 2.x CRI config (schema version 3). Enables
/// the CRI runtime plugin (built-in + enabled by default in the upstream static
/// tarball), the runc runtime via the v2 shim with the cgroupfs driver (no
/// systemd on this node), and an explicit runc path on the boot partition.
/// `root` is persistent (EPHEMERAL → /var); `state` is volatile (/run tmpfs).
pub fn containerd_config_toml(rt: &RuntimeSection) -> String {
    format!(
        r#"version = 3
root = "/var/lib/containerd"
state = "/run/containerd"

[grpc]
  address = "{socket}"

[plugins.'io.containerd.cri.v1.runtime']
  [plugins.'io.containerd.cri.v1.runtime'.containerd]
    default_runtime_name = "runc"

    [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc]
      runtime_type = "io.containerd.runc.v2"

      [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc.options]
        SystemdCgroup = false
        BinaryName = "/boot/bin/runc"
"#,
        socket = rt.socket
    )
}
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p machined-config && cargo clippy -p machined-config --all-targets -- -D warnings && cargo fmt --all`
Expected: PASS — the config-gen test + all existing config tests (any test asserting the old `version = 2` stub must be updated to the new content; if one exists, fix its expectation to match the v3 config — report it).

- [ ] **Step 5: Commit**

```bash
git add crates/config/src/runtime_svc.rs crates/config/Cargo.toml
git commit -m "feat(config): containerd v3 CRI config (runc, cgroupfs, /boot/bin/runc)"
```

---

### Task 4: /boot/bin on PATH

**Files:**
- Modify: `crates/machined/src/main.rs` (`default_path_if_unset` + its test)

- [ ] **Step 1: Update the existing test** — find the `default_path_if_unset` test (search `default_path`) and change its expected string to include `:/boot/bin`. If no test exists, add:

```rust
#[test]
fn default_path_includes_boot_bin() {
    // Unset/empty PATH gets a default that includes /boot/bin (so the
    // supervised containerd finds its shim + runc there).
    let p = default_path_if_unset(None).unwrap();
    assert!(p.ends_with("/boot/bin"), "{p}");
    assert!(p.contains(":/sbin:"), "keeps the standard dirs: {p}");
    // A non-empty PATH is left alone.
    assert!(default_path_if_unset(Some(std::ffi::OsStr::new("/x"))).is_none());
}
```

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined default_path`
Expected: FAIL — current default ends `/sbin:/bin`, not `/boot/bin`.

- [ ] **Step 3: Implement** — append `:/boot/bin` to the default string in `default_path_if_unset` (main.rs:216):

```rust
        _ => Some("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/boot/bin"),
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p machined default_path`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/machined/src/main.rs
git commit -m "feat(machined): add /boot/bin to PID1 default PATH (containerd shim + runc)"
```

---

### Task 5: Mount cgroup2

**Files:**
- Modify: `crates/platform/src/lib.rs` (`essential_mounts` + cgroup2)
- Modify: `crates/platform/src/fake.rs` (`mounts()` accessor)

- [ ] **Step 1: Failing test** (platform crate tests)

```rust
#[test]
fn essential_mounts_include_cgroup2() {
    let p = FakePlatform::new();
    p.mount_essential().unwrap();
    let m = p.mounts();
    assert!(
        m.iter().any(|s| s.target == "/sys/fs/cgroup" && s.fstype == "cgroup2"),
        "cgroup2 must be mounted at /sys/fs/cgroup: {m:?}"
    );
    // /sys is still mounted before cgroup (cgroup2 lives under it).
    let sys = m.iter().position(|s| s.target == "/sys");
    let cg = m.iter().position(|s| s.target == "/sys/fs/cgroup");
    assert!(sys < cg, "/sys must mount before /sys/fs/cgroup");
}
```

If `FakePlatform` has no `mounts()` accessor yet, this won't compile — add it in Step 3.

- [ ] **Step 2: Run, verify it fails**

Run: `cargo test -p machined-platform cgroup2`
Expected: FAIL — no cgroup2 mount (and/or `mounts()` missing).

- [ ] **Step 3: Implement**

In `crates/platform/src/fake.rs`, add the accessor (mirroring `modules_loaded()`):

```rust
/// Mounts issued, in call order (test inspection).
pub fn mounts(&self) -> Vec<MountSpec> {
    self.recorded.lock().unwrap().mounts.clone()
}
```

In `crates/platform/src/lib.rs` `essential_mounts()`, append the cgroup2 mount AFTER sysfs (it nests under `/sys`):

```rust
    vec![
        m("proc", "/proc", "proc"),
        m("sysfs", "/sys", "sysfs"),
        m("devtmpfs", "/dev", "devtmpfs"),
        m("tmpfs", "/run", "tmpfs"),
        m("tmpfs", "/tmp", "tmpfs"),
        // cgroup v2 unified hierarchy — containerd/runc need it for container
        // cgroups. (RuntimeReady itself doesn't require it, but a functional
        // runtime does; controller subtree-delegation is deferred to the
        // pod-launch milestone.)
        m("cgroup2", "/sys/fs/cgroup", "cgroup2"),
    ]
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test -p machined-platform && cargo clippy -p machined-platform --all-targets -- -D warnings && cargo fmt --all`
Expected: PASS — the cgroup2 test + the existing `mount_essential_skips_already_mounted` test (now covering 6 mounts; if that test counts mounts, update its expected count).

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/lib.rs crates/platform/src/fake.rs
git commit -m "feat(platform): mount cgroup2 unified hierarchy at /sys/fs/cgroup"
```

---

### Task 6: Enable the runtime in node-ci.yaml

**Files:**
- Modify: `examples/node-ci.yaml`

- [ ] **Step 1: Edit** — replace the runtime block:

```yaml
  runtime:
    disabled: false
    binary: /boot/bin/containerd
```

(The `socket`/`config_path` defaults are correct: `/run/containerd/containerd.sock`, `/etc/containerd/config.toml`.)

- [ ] **Step 2: Verify it still parses** — the imager's `ci_example_config_parses` test (in `crates/imager/src/build.rs`, added in M7a Task 11) `include_str!`s this file and runs `machined_config::load_from_str`. Run it:

Run: `cargo test -p machined-imager ci_example`
Expected: PASS — the runtime section with `disabled: false` + `binary` parses (RuntimeSection uses `deny_unknown_fields` + `default`, so only valid keys allowed; `binary` is a valid field).

- [ ] **Step 3: Commit**

```bash
git add examples/node-ci.yaml
git commit -m "feat(example): enable containerd runtime in node-ci.yaml (/boot/bin/containerd)"
```

---

### Task 7: Pin containerd + runc artifacts

**Files:**
- Modify: `crates/imager/artifacts.toml`

- [ ] **Step 1: Download + verify the real sha256** (you have network)

```bash
cd /tmp
# containerd static (no libc) — bin/{containerd,containerd-shim-runc-v2,ctr,…}
curl -sL -o containerd.tgz https://github.com/containerd/containerd/releases/download/v2.0.9/containerd-static-2.0.9-linux-amd64.tar.gz
sha256sum containerd.tgz
# sanity: it's a single-stream gzip tar with bin/containerd, all static ELF
tar -tzf containerd.tgz | head
tar -xzf containerd.tgz -C /tmp/cd-check && file /tmp/cd-check/bin/containerd   # expect "statically linked"

# runc static-pie binary
curl -sL -o runc.amd64 https://github.com/opencontainers/runc/releases/download/v1.4.3/runc.amd64
sha256sum runc.amd64
file runc.amd64   # expect "static-pie linked"
# cross-check against the release's signed sums:
curl -sL https://github.com/opencontainers/runc/releases/download/v1.4.3/runc.sha256sum | grep runc.amd64
```

Use the **computed** sha256 values (NOT any pre-supplied hash). If v2.0.9 / v1.4.3 have been superseded or the asset 404s, pick the current stable patch of the same line and re-verify (containerd 2.x schema is identical; runc ≥1.4 stable). Confirm `tar -tzf` shows `bin/containerd` (the static tarball layout) and `file` reports static linkage for both — if either is dynamically linked, you have the wrong asset.

- [ ] **Step 2: Add the entries** to the `x86_64` list in `artifacts.toml` (after the existing apk entries), filling the real shas:

```toml
  { name = "containerd", url = "https://github.com/containerd/containerd/releases/download/v2.0.9/containerd-static-2.0.9-linux-amd64.tar.gz", sha256 = "<COMPUTED>", kind = "boot-tarball" },
  { name = "runc", url = "https://github.com/opencontainers/runc/releases/download/v1.4.3/runc.amd64", sha256 = "<COMPUTED>", kind = "boot-binary", rename = "runc" },
```

Add a comment noting these are GitHub-release static binaries staged to `/boot/bin` (not Alpine apks → initramfs).

- [ ] **Step 3: Commit**

```bash
git add crates/imager/artifacts.toml
git commit -m "feat(imager): pin containerd 2.0.9 static + runc 1.4.3 for /boot/bin"
```

---

### Task 8: Boot test asserts RuntimeReady

**Files:**
- Modify: `scripts/boot-test-x86_64.sh`

- [ ] **Step 1: Add the RuntimeStatus assertion** — the VolumeStatus loop currently ends with `BOOT TEST PASSED; exit 0`. Change it so that after the volumes are confirmed Provisioned, the script then asserts the runtime is ready BEFORE declaring success. Replace the volume-loop success tail:

```bash
echo "checking provisioned volumes (namespace block)..."
vol_deadline=$((SECONDS + 120))
volumes_ok=0
while [ $SECONDS -lt $vol_deadline ]; do
  VOLS=$(ctl get VolumeStatus --namespace block 2>/dev/null || true)
  if echo "$VOLS" | grep -Eq 'name=STATE .*phase=Provisioned' \
     && echo "$VOLS" | grep -Eq 'name=EPHEMERAL .*phase=Provisioned'; then
    echo "$VOLS"; volumes_ok=1; break
  fi
  sleep 2
done
if [ "$volumes_ok" -ne 1 ]; then
  echo "volumes never provisioned"; tail -80 "$SERIAL"; exit 1
fi

echo "checking runtime readiness (namespace runtime)..."
rt_deadline=$((SECONDS + 120))
while [ $SECONDS -lt $rt_deadline ]; do
  RT=$(ctl get RuntimeStatus --namespace runtime 2>/dev/null || true)
  if echo "$RT" | grep -Eq 'ready=true'; then
    echo "$RT"; echo "BOOT TEST PASSED"; exit 0
  fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -80 "$SERIAL"; exit 1; fi
  sleep 2
done
echo "runtime never became ready:"; ctl get RuntimeStatus --namespace runtime || true
tail -120 "$SERIAL"; exit 1
```

Confirm the `RuntimeStatus` row format from `crates/apiserver/src/mapping.rs` (`ready=true name=containerd version=…`) — grep for `ready=true` is robust.

- [ ] **Step 2: Run locally if QEMU available** (you validated the GHCR image earlier; you can reuse it)

Run: `docker run --rm --device /dev/kvm -v "$PWD":/work -w /work machined-ci:local make boot-test`
Expected: builds the image (now downloading containerd+runc — ~65 MB, cached after first run), boots, and prints `RuntimeStatus … ready=true` then `BOOT TEST PASSED`. If `ready=true` never appears:
- `grep -iE "containerd|cri|runc|cgroup|oom|plugin" target/boot-test/serial.log` — common causes: containerd config rejected (TOML/plugin error → containerd exits, restart-loops with backoff), `/boot/bin/containerd` not executable or not found (PATH), CRI plugin disabled, or OOM at `-m 512` (containerd's startup spike — if the serial shows the oom-killer, bump QEMU `-m` to 1024 in the script and note it).
- These are EXPECTED first-boot integration findings; fix root causes in config/PATH/mount (not by weakening the assertion). If a failure points at a code bug in a committed task, STOP and report it for a fix round rather than patching the script.

- [ ] **Step 3: Commit**

```bash
git add scripts/boot-test-x86_64.sh
git commit -m "test(boot): assert RuntimeStatus ready=true (CRI plugin loaded)"
```

---

### Task 9: Gates + finish

- [ ] **Step 1: Full gate**

Run: `make pre-commit`
Expected: clean (fmt + clippy -D warnings + workspace test).

- [ ] **Step 2: Finish**

Follow superpowers:finishing-a-development-branch. PR → CI. The in-container `boot-test` job is the integration gate: it builds the full-stack image (containerd+runc on `/boot/bin`), boots it in QEMU/KVM, and now asserts `RuntimeStatus.ready=true` in addition to the volumes. If CI's boot-test fails where the local run passed, pull the serial-log artifact (uploaded on failure) and diagnose. Merge to main, delete branch, confirm main CI green.

---

## Verification (end-to-end)

1. `cargo test --workspace` green: imager boot-staging (3) + build-pipeline boot assertion; config v3-gen; machined default-path; platform cgroup2; node-ci parse.
2. `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all --check` clean.
3. **The bar:** CI boot-test boots the full-stack image and `machinectl get RuntimeStatus --namespace runtime` shows `ready=true` — proving containerd started from `/boot/bin`, loaded its CRI plugin with the generated v3 config, and answered the CRI `Status` RPC. Plus the existing VolumeStatus + API assertions.

## Known gaps / deferred (documented, not in scope)

- **cgroup controller delegation + PID1 leaf-move** (`echo +cpu +memory +pids +io > /sys/fs/cgroup/cgroup.subtree_control`, move PID1 to `init.scope`) — NOT needed for `RuntimeReady` (hardcoded in containerd source); REQUIRED only to actually launch a pod with limits. Deferred to the pod-launch milestone.
- **overlay/veth/bridge kernel modules** (`=m` in the Alpine kernel) — needed for image layers (overlay snapshotter) + CNI; not for RuntimeReady. The boot test asserts the runtime is *ready*, not that a pod runs.
- **containerd root on persistent storage / ordering vs EPHEMERAL mount** — `root = /var/lib/containerd` assumes `/var` (EPHEMERAL) is mounted before containerd starts. containerd starts in the boot `services` phase, after the mount controller, so this usually holds; but it isn't gated like the M7b-1 STATE/PKI wait. For RuntimeReady (which doesn't use root) it's moot; if a long-running node needs guaranteed-persistent containerd data, gate the containerd service start on the EPHEMERAL `MountStatus` (a follow-up mirroring M7b-1's `wait_for_state_mount`).
- **QEMU memory** — if containerd's startup spike OOMs at `-m 512`, the boot test bumps to `-m 1024`; the 512 MB figure is the node-OS footprint target, not a containerd-loaded ceiling.
- **Warm-reboot CI assertion** (enabled by the M7b-1 race fix) — still a separate optional follow-up.
- **containerd 2.3.1 LTS** — schema-identical to 2.0.9; a drop-in version bump if longer support is wanted.
