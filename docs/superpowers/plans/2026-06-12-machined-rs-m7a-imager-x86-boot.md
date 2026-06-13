# M7a — Imager + x86_64 QEMU Boot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `machined-imager` CLI that builds a bootable x86_64 disk image (machined as PID 1) entirely in userspace, plus a QEMU boot test in CI that asserts the node comes up (mTLS API answers, STATE+EPHEMERAL provisioned).

**Architecture:** New `crates/imager` downloads pinned Alpine artifacts (kernel+modules apk, musl, e2fsprogs apks), assembles a gzip cpio initramfs with static-musl machined as `/init`, and writes a GPT image with one FAT partition (label `EFI`) holding kernel, initramfs, config.yaml, and optional pre-baked PKI. machined gains pid1-only early-boot steps (load modules → mount `/boot` → seed PKI → read config from `/boot/config.yaml`) and the provisioner gains a `CompleteLayout` decision (disk with exactly `{EFI}` → append+format STATE/EPHEMERAL, EFI never touched). CI boots the image in QEMU (`-kernel/-initrd` direct boot, KVM) and drives it with machinectl through a port-forward.

**Tech Stack:** existing `gpt` crate (GPT write), `fatfs`+`fscommon` (FAT population), `flate2`+`tar` (apk extraction, initramfs gzip), `ureq`+`sha2`+`hex` (pinned downloads), `toml` (manifest), `nix` kmod (module loading), QEMU + KVM in CI.

**Plan-time verified facts (do not re-derive):**
- Alpine v3.21 `linux-virt` kernel config: `VIRTIO_BLK=m, VIRTIO_NET=m, EXT4_FS=m, VFAT_FS=m, NLS_CODEPAGE_437=m, NLS_ISO8859_1=m`; `VIRTIO_PCI=y, DEVTMPFS_MOUNT=y, RD_GZIP=y, SERIAL_8250_CONSOLE=y, EFI_PARTITION=y`. So the initramfs MUST carry those six modules (+dep closure); gzip cpio is fine; serial console works. Alpine modules are gzipped (`.ko.gz`) — the imager decompresses them at extraction (no reliance on kernel `MODULE_DECOMPRESS`).
- The provisioner guard (`crates/controllers/src/block/provision.rs:37`) demands exact `{EFI,STATE,EPHEMERAL}` label equality; a flashed image (EFI only) is `RefuseForeign` today → Task 9 adds `CompleteLayout`.
- The API binds `127.0.0.1:50000` (`crates/machined/src/main.rs` in `run_daemon`); QEMU hostfwd delivers to the guest's NIC IP, so it must bind `0.0.0.0` (Task 8).
- `VolumeMountController` is already idempotent (`is_mounted` check) — pre-mounted `/boot` is fine.
- machinectl pins TLS `domain_name("127.0.0.1")` and server certs carry SAN `127.0.0.1` — host→hostfwd connections verify fine.
- `Platform::mount_essential()` (platform/src/lib.rs) loops `mount` with no is_mounted guard — Task 8 makes it idempotent so it can run both pre-config and in the boot sequence.

---

### Task 1: Imager crate scaffold

**Files:**
- Modify: `Cargo.toml` (workspace members + deps)
- Create: `crates/imager/Cargo.toml`, `crates/imager/src/main.rs`

- [ ] **Step 1: Register workspace member and deps**

In root `Cargo.toml`: add `"crates/imager"` to `members`. Add to `[workspace.dependencies]`:

```toml
ureq = "2"
sha2 = "0.10"
hex = "0.4"
toml = "0.8"
flate2 = "1"
tar = "0.4"
fatfs = "0.3"
fscommon = "0.1"
tempfile = "3"
machined-pki = { path = "crates/pki" }
```

(`machined-pki` may already be listed — only add if absent.)

- [ ] **Step 2: Crate manifest**

`crates/imager/Cargo.toml`:

```toml
[package]
name = "machined-imager"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "machined-imager"
path = "src/main.rs"

[dependencies]
anyhow.workspace = true
clap.workspace = true
ureq.workspace = true
sha2.workspace = true
hex.workspace = true
toml.workspace = true
serde.workspace = true
flate2.workspace = true
tar.workspace = true
gpt.workspace = true
fatfs.workspace = true
fscommon.workspace = true
machined-pki.workspace = true
machined-config = { path = "../config" }

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 3: CLI skeleton**

`crates/imager/src/main.rs`:

```rust
//! machined-imager — builds bootable machined disk images in pure userspace.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod apk;
mod cpio;
mod fetch;
mod image;
mod initramfs;
mod manifest;
mod modules;
mod pki;

#[derive(Parser)]
#[command(name = "machined-imager", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build a bootable disk image.
    Build {
        /// Target architecture.
        #[arg(long, value_parser = ["x86_64"])]
        arch: String,
        /// Path to the static machined binary (musl).
        #[arg(long)]
        machined: PathBuf,
        /// Machine config YAML to embed (validated before embedding).
        #[arg(long)]
        config: PathBuf,
        /// Output image path.
        #[arg(long)]
        out: PathBuf,
        /// Image size in bytes (sparse). Default 4 GiB.
        #[arg(long, default_value_t = 4 * 1024 * 1024 * 1024)]
        size: u64,
        /// Optional pre-generated PKI dir (ca.pem, ca.key, server.pem, server.key)
        /// copied to pki/ on the boot partition.
        #[arg(long)]
        pki_dir: Option<PathBuf>,
        /// Also copy kernel + initramfs to this dir (for QEMU -kernel boot).
        #[arg(long)]
        emit_boot: Option<PathBuf>,
        /// Artifact manifest path.
        #[arg(long, default_value = "crates/imager/artifacts.toml")]
        manifest: PathBuf,
        /// Download cache dir.
        #[arg(long, default_value = "target/imager-cache")]
        cache: PathBuf,
    },
    /// Generate a node PKI dir (CA + server identity + machinectl client bundle).
    GenPki {
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Build { .. } => anyhow::bail!("not implemented yet"),
        Command::GenPki { .. } => anyhow::bail!("not implemented yet"),
    }
}
```

Create empty module files `src/{apk,cpio,fetch,image,initramfs,manifest,modules,pki}.rs` each containing only a module doc comment (filled by later tasks). Comment out the `mod` lines for modules not yet implemented if clippy complains about empty files — simpler: create each with `//! <purpose>` only, which compiles.

- [ ] **Step 4: Gates + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo build -p machined-imager`
Expected: clean.

```bash
git add Cargo.toml crates/imager
git commit -m "feat(imager): scaffold machined-imager crate + CLI"
```

---

### Task 2: Artifact manifest + verified fetcher

**Files:**
- Create: `crates/imager/artifacts.toml`, fill `crates/imager/src/manifest.rs`, `crates/imager/src/fetch.rs`

- [ ] **Step 1: Write failing manifest test** (bottom of `manifest.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_and_selects_arch() {
        let m: Manifest = toml::from_str(
            r#"
[[artifact.x86_64]]
name = "linux-virt"
url = "https://example.org/linux-virt.apk"
sha256 = "aa"
kind = "apk"
"#,
        )
        .unwrap();
        let arts = m.for_arch("x86_64").unwrap();
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].name, "linux-virt");
        assert!(m.for_arch("riscv").is_none());
    }
}
```

- [ ] **Step 2: Implement manifest.rs**

```rust
//! The pinned-artifact manifest (artifacts.toml): every external input to an
//! image is named here with URL + sha256. Nothing unpinned is ever downloaded.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// arch → artifact list.
    pub artifact: BTreeMap<String, Vec<Artifact>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub url: String,
    pub sha256: String,
    /// "apk" (extracted into the initramfs rootfs) — the only kind in M7a.
    pub kind: String,
}

impl Manifest {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }
    pub fn for_arch(&self, arch: &str) -> Option<&[Artifact]> {
        self.artifact.get(arch).map(|v| v.as_slice())
    }
}
```

- [ ] **Step 3: Write failing fetch tests** (bottom of `fetch.rs`) — checksum gate is the security property:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    struct StaticFetcher(Vec<u8>);
    impl Fetch for StaticFetcher {
        fn get(&self, _url: &str) -> anyhow::Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn good_checksum_is_cached_and_returned() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"payload".to_vec();
        let sum = hex::encode(Sha256::digest(&body));
        let p = fetch_verified(&StaticFetcher(body.clone()), "http://x/a", &sum, dir.path()).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), body);
        // Second call hits cache: a fetcher that would now fail is never asked.
        struct Bomb;
        impl Fetch for Bomb {
            fn get(&self, _u: &str) -> anyhow::Result<Vec<u8>> {
                panic!("must not re-download")
            }
        }
        let p2 = fetch_verified(&Bomb, "http://x/a", &sum, dir.path()).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn bad_checksum_is_a_hard_error_and_nothing_is_cached() {
        let dir = tempfile::tempdir().unwrap();
        let err = fetch_verified(&StaticFetcher(b"evil".to_vec()), "http://x/a", &"00".repeat(32), dir.path())
            .unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
```

- [ ] **Step 4: Implement fetch.rs**

```rust
//! Checksum-verified, cached downloads. The cache key IS the pinned sha256, so
//! a cache hit is by definition verified content.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub trait Fetch {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>>;
}

pub struct HttpFetcher;

impl Fetch for HttpFetcher {
    fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let resp = ureq::get(url).call()?;
        let mut buf = Vec::new();
        resp.into_reader().read_to_end(&mut buf)?;
        Ok(buf)
    }
}

/// Return a path to the verified artifact, downloading only on cache miss.
/// A checksum mismatch is a hard error and leaves no cache entry behind.
pub fn fetch_verified(
    fetcher: &dyn Fetch,
    url: &str,
    sha256: &str,
    cache_dir: &Path,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(cache_dir)?;
    let cached = cache_dir.join(sha256);
    if cached.exists() {
        return Ok(cached);
    }
    let body = fetcher.get(url)?;
    let actual = hex::encode(Sha256::digest(&body));
    if actual != sha256 {
        anyhow::bail!("sha256 mismatch for {url}: expected {sha256}, got {actual}");
    }
    // Write-then-rename so a crash never leaves a half-written "verified" file.
    let tmp = cache_dir.join(format!(".{sha256}.tmp"));
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, &cached)?;
    Ok(cached)
}
```

Note: `use std::io::Read;` is needed for `read_to_end`.

- [ ] **Step 5: Placeholder manifest file**

`crates/imager/artifacts.toml` (real URLs+checksums pinned in Task 11 — commit the file now with this header comment and an empty x86_64 list so `Manifest::load` works):

```toml
# Pinned external artifacts. Every input to an image build is named here with
# url + sha256; the fetcher refuses anything else. Re-pin deliberately.
# kind = "apk": Alpine package, extracted into the initramfs rootfs.

[artifact]
x86_64 = []
```

- [ ] **Step 6: Gates + commit**

Run: `cargo test -p machined-imager && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`
Expected: 3 tests pass.

```bash
git add crates/imager
git commit -m "feat(imager): pinned-artifact manifest + checksum-verified cached fetcher"
```

---

### Task 3: apk extraction

Alpine `.apk` = concatenated gzip streams (signature, control, data) of tar archives. `flate2::read::MultiGzDecoder` + `tar` reads all entries in sequence; payload entries are the ones not starting with `.` (metadata entries are `.SIGN.*`, `.PKGINFO`, etc.). Gzipped kernel modules (`*.ko.gz`) are decompressed during extraction.

**Files:**
- Fill: `crates/imager/src/apk.rs`

- [ ] **Step 1: Write failing test** (bottom of `apk.rs`). Build a synthetic apk in the test: a gzipped tar with `.PKGINFO`, a regular file, a directory, and a `.ko.gz` member:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn synthetic_apk() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h.clone(), ".PKGINFO", &b"meta"[..]).unwrap();
            let mut hd = tar::Header::new_gnu();
            hd.set_size(0);
            hd.set_entry_type(tar::EntryType::Directory);
            hd.set_mode(0o755);
            hd.set_cksum();
            b.append_data(&mut hd, "sbin/", &b""[..]).unwrap();
            let mut hf = tar::Header::new_gnu();
            hf.set_size(5);
            hf.set_mode(0o755);
            hf.set_cksum();
            b.append_data(&mut hf, "sbin/mkfs.ext4", &b"\x7fELF!"[..]).unwrap();
            // a gzipped kernel module
            let mut ko = Vec::new();
            GzEncoder::new(&mut ko, Compression::default())
                .write_all(b"module-bytes")
                .unwrap();
            let mut hk = tar::Header::new_gnu();
            hk.set_size(ko.len() as u64);
            hk.set_mode(0o644);
            hk.set_cksum();
            b.append_data(&mut hk, "lib/modules/6.12.81-0-virt/kernel/fs/ext4/ext4.ko.gz", &ko[..])
                .unwrap();
            b.finish().unwrap();
        }
        let mut gz = Vec::new();
        GzEncoder::new(&mut gz, Compression::default())
            .write_all(&tar_bytes)
            .unwrap();
        gz
    }

    #[test]
    fn extracts_payload_skips_metadata_decompresses_ko_gz() {
        let dir = tempfile::tempdir().unwrap();
        let apk = dir.path().join("a.apk");
        std::fs::write(&apk, synthetic_apk()).unwrap();
        extract_apk(&apk, dir.path().join("root").as_path()).unwrap();
        let root = dir.path().join("root");
        assert!(!root.join(".PKGINFO").exists(), "metadata must be skipped");
        assert_eq!(std::fs::read(root.join("sbin/mkfs.ext4")).unwrap(), b"\x7fELF!");
        // .ko.gz arrives decompressed, with the .gz suffix stripped
        assert_eq!(
            std::fs::read(root.join("lib/modules/6.12.81-0-virt/kernel/fs/ext4/ext4.ko")).unwrap(),
            b"module-bytes"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join("sbin/mkfs.ext4")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "exec bit must survive");
        }
    }
}
```

- [ ] **Step 2: Implement apk.rs**

```rust
//! Alpine .apk extraction: concatenated gzip tar streams; payload entries are
//! everything not starting with '.'. Gzipped modules are decompressed so the
//! node never needs kernel module decompression support.

use flate2::read::{GzDecoder, MultiGzDecoder};
use std::io::Read;
use std::path::Path;

pub fn extract_apk(apk: &Path, rootfs: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(apk)?;
    let gz = MultiGzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(first) = path.components().next() else { continue };
        if first.as_os_str().to_string_lossy().starts_with('.') {
            continue; // .SIGN.*, .PKGINFO and friends
        }
        if path.extension().is_some_and(|e| e == "gz")
            && path.to_string_lossy().ends_with(".ko.gz")
        {
            let target = rootfs.join(path.with_extension("")); // strip .gz
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut raw = Vec::new();
            GzDecoder::new(&mut entry).read_to_end(&mut raw)?;
            std::fs::write(&target, raw)?;
        } else {
            entry.unpack_in(rootfs)?;
        }
    }
    Ok(())
}
```

`unpack_in` refuses path traversal (`../`) by design — that property is why we use it instead of joining paths by hand.

- [ ] **Step 3: Gates + commit**

Run: `cargo test -p machined-imager apk && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`

```bash
git add crates/imager/src/apk.rs
git commit -m "feat(imager): apk extraction (multi-gz tar, metadata skip, .ko.gz decompress)"
```

---

### Task 4: Kernel module closure resolver

`modules.dep` lines look like `kernel/drivers/block/virtio_blk.ko.gz: kernel/drivers/virtio/virtio.ko.gz ...` (paths relative to `/lib/modules/<ver>`). Given root module names, produce the dependency-closure as an ORDERED list (dependencies first) so machined can load them blindly in file order. Resolver matches `.ko` and `.ko.gz` interchangeably (the imager stripped `.gz` on disk).

**Files:**
- Fill: `crates/imager/src/modules.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const DEP: &str = "\
kernel/drivers/block/virtio_blk.ko.gz: kernel/drivers/virtio/virtio.ko.gz
kernel/drivers/virtio/virtio.ko.gz:
kernel/fs/ext4/ext4.ko.gz: kernel/fs/jbd2/jbd2.ko.gz kernel/lib/crc16.ko.gz
kernel/fs/jbd2/jbd2.ko.gz:
kernel/lib/crc16.ko.gz:
kernel/fs/fat/vfat.ko.gz: kernel/fs/fat/fat.ko.gz
kernel/fs/fat/fat.ko.gz:
";

    #[test]
    fn closure_is_dep_ordered_and_deduped() {
        let order = resolve_closure(DEP, &["virtio_blk", "ext4"]).unwrap();
        let pos = |n: &str| order.iter().position(|p| p.ends_with(&format!("/{n}.ko"))).unwrap();
        assert!(pos("virtio") < pos("virtio_blk"));
        assert!(pos("jbd2") < pos("ext4"));
        assert!(pos("crc16") < pos("ext4"));
        assert_eq!(order.len(), 5, "no duplicates, nothing extra: {order:?}");
        assert!(order.iter().all(|p| p.ends_with(".ko")), "gz suffix stripped");
    }

    #[test]
    fn unknown_module_is_an_error() {
        let err = resolve_closure(DEP, &["nvme"]).unwrap_err();
        assert!(err.to_string().contains("nvme"), "{err}");
    }
}
```

- [ ] **Step 2: Implement modules.rs**

```rust
//! modules.dep closure resolution: from root module names to a dependency-
//! ordered list of .ko paths (relative to /lib/modules/<ver>), so the node can
//! finit_module() them in file order with zero dep logic at boot.

use std::collections::{BTreeMap, BTreeSet};

fn strip_gz(p: &str) -> String {
    p.strip_suffix(".gz").unwrap_or(p).to_string()
}

fn module_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    strip_gz(base).trim_end_matches(".ko").replace('-', "_")
}

/// Resolve `roots` (bare module names, '-'/'_' insensitive) against a
/// modules.dep text. Returns .ko paths (gz-stripped), dependencies first.
pub fn resolve_closure(modules_dep: &str, roots: &[&str]) -> anyhow::Result<Vec<String>> {
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new(); // path -> dep paths
    let mut by_name: BTreeMap<String, String> = BTreeMap::new(); // name -> path
    for line in modules_dep.lines() {
        let Some((module, rest)) = line.split_once(':') else { continue };
        let path = strip_gz(module.trim());
        by_name.insert(module_name(&path), path.clone());
        deps.insert(
            path,
            rest.split_whitespace().map(strip_gz).collect(),
        );
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    fn visit(
        path: &str,
        deps: &BTreeMap<String, Vec<String>>,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(path.to_string()) {
            return;
        }
        for d in deps.get(path).map(|v| v.as_slice()).unwrap_or(&[]) {
            visit(d, deps, seen, out);
        }
        out.push(path.to_string());
    }
    for root in roots {
        let name = root.replace('-', "_");
        let path = by_name
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("module {root} not found in modules.dep"))?;
        visit(path, &deps, &mut seen, &mut out);
    }
    Ok(out)
}

/// The module roots an x86_64 QEMU/virtio boot needs (Alpine linux-virt builds
/// these =m): block + net + the three filesystems machined mounts.
pub const X86_64_QEMU_MODULES: &[&str] = &[
    "virtio_blk",
    "virtio_net",
    "ext4",
    "vfat",
    "nls_cp437",
    "nls_iso8859_1",
];
```

- [ ] **Step 3: Gates + commit**

Run: `cargo test -p machined-imager modules && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`

```bash
git add crates/imager/src/modules.rs
git commit -m "feat(imager): modules.dep closure resolver (dep-ordered, gz-stripped)"
```

---

### Task 5: cpio (newc) writer

Hand-rolled `newc` writer (~80 lines, fully testable; avoids an unmaintained dep). Must support: regular files (with mode), directories, and the `/dev/console` character device (c 5:1) — without it the kernel runs `/init` with no stdio and the serial console stays silent.

**Files:**
- Fill: `crates/imager/src/cpio.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn field(archive: &[u8], entry_off: usize, n: usize) -> u32 {
        // header: 6 magic + 13 8-hex fields; field n at 6 + n*8
        let s = std::str::from_utf8(&archive[entry_off + 6 + n * 8..entry_off + 6 + (n + 1) * 8]).unwrap();
        u32::from_str_radix(s, 16).unwrap()
    }

    #[test]
    fn writes_parseable_newc_with_device_node_and_trailer() {
        let mut w = CpioWriter::new();
        w.dir("dev", 0o755);
        w.char_device("dev/console", 0o600, 5, 1);
        w.file("init", 0o755, b"#!ELF");
        let bytes = w.finish();
        assert_eq!(&bytes[0..6], b"070701", "newc magic");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("dev/console"));
        assert!(text.contains("TRAILER!!!"));
        assert_eq!(bytes.len() % 4, 0, "archive padded to 4");
        // first entry is the dir: mode field (index 1) = S_IFDIR | 0755
        assert_eq!(field(&bytes, 0, 1), 0o040755);
    }

    #[test]
    fn file_data_is_embedded_and_padded() {
        let mut w = CpioWriter::new();
        w.file("a", 0o644, b"xyz");
        let bytes = w.finish();
        let pos = bytes.windows(3).position(|w| w == b"xyz").unwrap();
        assert_eq!(pos % 4, 0, "data 4-aligned");
    }
}
```

- [ ] **Step 2: Implement cpio.rs**

```rust
//! Minimal cpio "newc" (070701) archive writer — just what an initramfs needs:
//! directories, regular files, and character devices. Format: 6-byte magic +
//! 13 zero-padded 8-hex fields, NUL-terminated name, name and data each padded
//! to 4 bytes; terminated by the TRAILER!!! entry.

pub struct CpioWriter {
    buf: Vec<u8>,
    ino: u32,
}

impl CpioWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new(), ino: 1 }
    }

    fn entry(&mut self, name: &str, mode: u32, rmajor: u32, rminor: u32, data: &[u8]) {
        let ino = self.ino;
        self.ino += 1;
        let fields: [u32; 13] = [
            ino,                 // ino
            mode,                // mode (incl. file type bits)
            0,                   // uid
            0,                   // gid
            1,                   // nlink
            0,                   // mtime (0 = reproducible builds)
            data.len() as u32,   // filesize
            0,                   // devmajor
            0,                   // devminor
            rmajor,              // rdevmajor
            rminor,              // rdevminor
            name.len() as u32 + 1, // namesize incl. NUL
            0,                   // check (always 0 for newc)
        ];
        self.buf.extend_from_slice(b"070701");
        for f in fields {
            self.buf.extend_from_slice(format!("{f:08X}").as_bytes());
        }
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(0);
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
        self.buf.extend_from_slice(data);
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
    }

    pub fn dir(&mut self, name: &str, perm: u32) {
        self.entry(name, 0o040000 | perm, 0, 0, &[]);
    }
    pub fn file(&mut self, name: &str, perm: u32, data: &[u8]) {
        self.entry(name, 0o100000 | perm, 0, 0, data);
    }
    pub fn char_device(&mut self, name: &str, perm: u32, major: u32, minor: u32) {
        self.entry(name, 0o020000 | perm, major, minor, &[]);
    }
    pub fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, 0, 0, &[]);
        self.buf
    }
}
```

- [ ] **Step 3: Gates + commit**

Run: `cargo test -p machined-imager cpio && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`

```bash
git add crates/imager/src/cpio.rs
git commit -m "feat(imager): newc cpio writer (files, dirs, char devices; reproducible)"
```

---

### Task 6: Initramfs assembly

Compose: rootfs dir tree (from extracted apks) + machined as `/init` + `/dev/console` node + `/etc/machined/modules.load` (the resolved, ordered module list as absolute paths) → gzip'd cpio.

**Files:**
- Fill: `crates/imager/src/initramfs.rs`

- [ ] **Step 1: Write failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    #[test]
    fn builds_gzip_cpio_with_init_console_and_modules_load() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("sbin")).unwrap();
        std::fs::write(rootfs.join("sbin/mkfs.ext4"), b"elf").unwrap();
        let machined = dir.path().join("machined");
        std::fs::write(&machined, b"machined-elf").unwrap();

        let bytes = build_initramfs(
            &rootfs,
            &machined,
            &["kernel/fs/ext4/ext4.ko".into(), "kernel/drivers/block/virtio_blk.ko".into()],
            "6.12.81-0-virt",
        )
        .unwrap();

        let mut raw = Vec::new();
        GzDecoder::new(&bytes[..]).read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("init"));
        assert!(text.contains("dev/console"));
        assert!(text.contains("sbin/mkfs.ext4"));
        assert!(text.contains("etc/machined/modules.load"));
        // modules.load content: absolute, ordered paths
        let want = "/lib/modules/6.12.81-0-virt/kernel/fs/ext4/ext4.ko\n/lib/modules/6.12.81-0-virt/kernel/drivers/block/virtio_blk.ko\n";
        assert!(text.contains(want), "ordered absolute module paths embedded");
        assert!(text.contains("TRAILER!!!"));
    }
}
```

- [ ] **Step 2: Implement initramfs.rs**

```rust
//! Assembles the initramfs: the extracted apk rootfs + machined as /init +
//! /dev/console + the ordered module list machined loads at early boot.

use crate::cpio::CpioWriter;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::Write;
use std::path::Path;

/// Build a gzip'd newc cpio. `module_paths` are .ko paths relative to
/// /lib/modules/<kver>, already dependency-ordered (Task 4).
pub fn build_initramfs(
    rootfs: &Path,
    machined: &Path,
    module_paths: &[String],
    kver: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut w = CpioWriter::new();
    for d in ["dev", "proc", "sys", "run", "tmp", "etc", "etc/machined", "boot", "system", "system/state", "var"] {
        w.dir(d, 0o755);
    }
    w.char_device("dev/console", 0o600, 5, 1);
    w.file("init", 0o755, &std::fs::read(machined)?);
    let modules_load: String = module_paths
        .iter()
        .map(|p| format!("/lib/modules/{kver}/{p}\n"))
        .collect();
    w.file("etc/machined/modules.load", 0o644, modules_load.as_bytes());
    add_tree(&mut w, rootfs, rootfs)?;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&w.finish())?;
    Ok(gz.finish()?)
}

fn add_tree(w: &mut CpioWriter, root: &Path, dir: &Path) -> anyhow::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name()); // deterministic archives
    for entry in entries {
        let path = entry.path();
        let rel = path.strip_prefix(root)?.to_string_lossy().to_string();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            w.dir(&rel, 0o755);
            add_tree(w, root, &path)?;
        } else if meta.is_file() {
            use std::os::unix::fs::PermissionsExt;
            w.file(&rel, meta.permissions().mode() & 0o7777, &std::fs::read(&path)?);
        }
        // symlinks: Alpine apks for our package set carry none we need; skipped.
    }
    Ok(())
}
```

NOTE for implementer: e2fsprogs' `mkfs.ext4` IS commonly a symlink to `mke2fs` inside the apk. If extraction shows that (check with `tar -tzvf` on the real apk in Task 11), add symlink support: `tar::EntryType::Symlink` in `apk.rs` unpacks fine via `unpack_in` (it handles symlinks); cpio then needs a `symlink` entry type — add `pub fn symlink(&mut self, name: &str, target: &str)` writing mode `0o120777` with the target as data, and handle `meta.file_type().is_symlink()` in `add_tree` (use `std::fs::symlink_metadata` — the code above uses `entry.metadata()` which FOLLOWS links; switch to `entry.path().symlink_metadata()?`). Write the symlink test then.

- [ ] **Step 3: Gates + commit**

Run: `cargo test -p machined-imager initramfs && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`

```bash
git add crates/imager/src/initramfs.rs
git commit -m "feat(imager): initramfs assembly (rootfs tree + /init + modules.load, deterministic)"
```

---

### Task 7: GPT + FAT image writer

Sparse image file → protective MBR + GPT with one partition: `EFI`, type EfiSystem, 512 MiB at 1 MiB alignment (matches `fixed_layout()` in provision.rs so a later full re-provision yields the same geometry) → FAT32 via `fatfs` on an `fscommon::StreamSlice` → populate boot files.

**Files:**
- Fill: `crates/imager/src/image.rs`

- [ ] **Step 1: Write failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_has_efi_only_gpt_and_populated_fat() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("test.img");
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(staging.join("bin")).unwrap();
        std::fs::write(staging.join("config.yaml"), b"machine: {}\n").unwrap();
        std::fs::write(staging.join("vmlinuz"), b"kernel").unwrap();
        std::fs::write(staging.join("bin/tool"), b"t").unwrap();

        write_image(&img, 2 * 1024 * 1024 * 1024, &staging).unwrap();

        // GPT readable, exactly one partition, named EFI, type EFI system.
        let disk = gpt::GptConfig::new().writable(false).open(&img).unwrap();
        let parts = disk.partitions();
        assert_eq!(parts.len(), 1);
        let p = parts.values().next().unwrap();
        assert_eq!(p.name, "EFI");
        assert_eq!(p.part_type_guid, gpt::partition_types::EFI);

        // FAT region readable, files present with content, subdirs work.
        let file = std::fs::File::options().read(true).open(&img).unwrap();
        let (start, end) = (p.first_lba * 512, (p.last_lba + 1) * 512);
        let slice = fscommon::StreamSlice::new(file, start, end).unwrap();
        let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
        let names: Vec<String> = fs.root_dir().iter().map(|e| e.unwrap().file_name()).collect();
        assert!(names.contains(&"config.yaml".to_string()), "{names:?}");
        assert!(names.contains(&"vmlinuz".to_string()));
        use std::io::Read;
        let mut buf = String::new();
        fs.root_dir().open_file("config.yaml").unwrap().read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "machine: {}\n");
        let sub: Vec<String> = fs.root_dir().open_dir("bin").unwrap().iter()
            .map(|e| e.unwrap().file_name()).filter(|n| n != "." && n != "..").collect();
        assert_eq!(sub, vec!["tool".to_string()]);
    }
}
```

- [ ] **Step 2: Implement image.rs**

```rust
//! Userspace disk-image writer: protective MBR + GPT with a single 512 MiB
//! FAT32 partition labeled EFI, populated from a staging directory. STATE and
//! EPHEMERAL are deliberately absent — machined completes the layout on first
//! boot, sized to the real disk (CompleteLayout, provision.rs).

use std::io::{Read, Seek, Write};
use std::path::Path;

const LB: u64 = 512;
const EFI_SIZE: u64 = 512 * 1024 * 1024; // matches fixed_layout()

/// Create `img` (sparse, `size` bytes), lay out GPT+EFI, copy `staging`'s
/// tree into the FAT filesystem.
pub fn write_image(img: &Path, size: u64, staging: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(size >= EFI_SIZE + 4 * 1024 * 1024, "image size too small");
    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(img)?;
    file.set_len(size)?;

    // Protective MBR, then a fresh GPT with the single EFI partition.
    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(u32::try_from((size / LB) - 1).unwrap_or(0xFFFF_FFFF));
    mbr.overwrite_lba0(&mut file)?;
    let mut gdisk = gpt::GptConfig::new()
        .writable(true)
        .initialized(false)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .open(img)?;
    gdisk.update_partitions(std::collections::BTreeMap::new())?;
    gdisk.add_partition("EFI", EFI_SIZE, gpt::partition_types::EFI, 0, None)?;
    let parts = gdisk.partitions().clone();
    gdisk.write()?;
    let p = parts.values().next().expect("one partition");
    let (start, end) = (p.first_lba * LB, (p.last_lba + 1) * LB);

    // Format + populate the FAT region through a bounds-checked slice.
    let file = std::fs::File::options().read(true).write(true).open(img)?;
    let mut slice = fscommon::StreamSlice::new(file, start, end)?;
    fatfs::format_volume(
        &mut slice,
        fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat32)
            .volume_label(*b"EFI        "),
    )?;
    let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new())?;
    copy_tree(&fs.root_dir(), staging)?;
    Ok(())
}

fn copy_tree(dir: &fatfs::Dir<'_, fscommon::StreamSlice<std::fs::File>>, src: &Path) -> anyhow::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(src)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.metadata()?.is_dir() {
            let sub = dir.create_dir(&name)?;
            copy_tree(&sub, &entry.path())?;
        } else {
            let mut f = dir.create_file(&name)?;
            f.truncate()?;
            let mut data = Vec::new();
            std::fs::File::open(entry.path())?.read_to_end(&mut data)?;
            f.write_all(&data)?;
        }
    }
    Ok(())
}
```

API-fit note: `gpt` 3.1 / `fatfs` 0.3 / `fscommon` 0.1 signatures may differ in detail (e.g. `partitions()` return type, `ProtectiveMBR` constructor, `Dir` generic params). Mirror `crates/block/src/sysfs.rs:243-308` for the gpt calls — it is the same version and known-good. Adjust until the TEST passes; the test (read back GPT + FAT) is the contract, the snippet is the sketch.

- [ ] **Step 3: Gates + commit**

Run: `cargo test -p machined-imager image && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`

```bash
git add crates/imager/src/image.rs
git commit -m "feat(imager): userspace GPT+FAT32 image writer (EFI-only layout)"
```

---

### Task 8: gen-pki + PKI bundle helper + machined early image-boot steps

**Files:**
- Modify: `crates/pki/src/lib.rs` (new `write_client_bundle`)
- Modify: `crates/machined/src/main.rs` (use helper; bind 0.0.0.0; call early steps)
- Create: `crates/machined/src/imageboot.rs`
- Modify: `crates/platform/src/lib.rs` (+`load_module`, idempotent `mount_essential`), `crates/platform/src/real.rs` (or wherever `RealPlatform` lives), `crates/platform/src/fake.rs`
- Modify: root `Cargo.toml` nix features: add `"kmod"`
- Fill: `crates/imager/src/pki.rs`

- [ ] **Step 1: pki crate — failing test** (in `crates/pki/src/lib.rs` tests):

```rust
#[test]
fn write_client_bundle_emits_ca_cert_key() {
    let dir = tempfile::tempdir().unwrap(); // pki crate already uses tempfile in tests; if not, std::env::temp_dir pattern as in existing tests
    let pki = NodePki::load_or_generate(dir.path(), "node", &["127.0.0.1".into()]).unwrap();
    let bundle = dir.path().join("machinectl");
    write_client_bundle(&bundle, &pki, "machinectl").unwrap();
    for f in ["ca.pem", "client.pem", "client.key"] {
        assert!(bundle.join(f).exists(), "{f}");
    }
}
```

- [ ] **Step 2: Implement `write_client_bundle` in machined-pki** — move the logic from `crates/machined/src/main.rs:126-141` (it reads: create dir, issue_client, write ca.pem/client.pem/client.key) into the pki crate:

```rust
/// Write a client bundle (ca.pem, client.pem, client.key) for `cn` under `dir`.
pub fn write_client_bundle(dir: &Path, pki: &NodePki, cn: &str) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| PkiError::Io { path: dir.into(), source: e })?;
    let client = pki.issue_client(cn)?;
    // ... identical writes as main.rs today, plus the existing 0600 key-perm handling
}
```

Match the pki crate's existing error type and key-permission discipline (keys 0600 — see how `load_or_generate` writes keys, reuse the same helper). Update `crates/machined/src/main.rs` to call `machined_pki::write_client_bundle(&pki_dir.join("machinectl"), &pki, "machinectl")` and delete the local copy.

- [ ] **Step 3: imager gen-pki** (`crates/imager/src/pki.rs`):

```rust
//! gen-pki: produce a complete pre-baked node PKI dir on the build host.

use std::path::Path;

pub fn gen_pki(out: &Path) -> anyhow::Result<()> {
    let pki = machined_pki::NodePki::load_or_generate(out, "node", &["127.0.0.1".into(), "localhost".into()])?;
    machined_pki::write_client_bundle(&out.join("machinectl"), &pki, "machinectl")?;
    println!("PKI written to {}", out.display());
    Ok(())
}
```

Wire `Command::GenPki` in main.rs to call it. SANs match what `run_daemon` uses, so the baked server identity verifies for the CI client.

- [ ] **Step 4: Platform additions — failing tests first** (platform crate tests):

```rust
#[test]
fn mount_essential_skips_already_mounted() {
    let p = FakePlatform::new();
    p.mount(&essential_mounts()[0]).unwrap(); // /proc pre-mounted
    p.mount_essential().unwrap();
    // exactly one /proc mount recorded, not two
    let proc_mounts = p.mounts().iter().filter(|m| m.target == "/proc").count();
    assert_eq!(proc_mounts, 1);
}

#[test]
fn fake_records_module_loads() {
    let p = FakePlatform::new();
    p.load_module(std::path::Path::new("/lib/modules/x/ext4.ko")).unwrap();
    assert_eq!(p.modules_loaded(), vec!["/lib/modules/x/ext4.ko".to_string()]);
}
```

(Adapt to FakePlatform's existing recording style — it records disk ops in `disk_ops`; follow the same pattern for a `modules_loaded` accessor and however mounts are exposed.)

- [ ] **Step 5: Implement Platform changes**

In `crates/platform/src/lib.rs`:
- Trait: add `fn load_module(&self, path: &Path) -> Result<()>;`
- `mount_essential` default impl becomes idempotent:

```rust
fn mount_essential(&self) -> Result<()> {
    for spec in essential_mounts() {
        if !self.is_mounted(&spec.target)? {
            self.mount(&spec)?;
        }
    }
    Ok(())
}
```

Real platform (Linux): `nix::kmod::finit_module(&std::fs::File::open(path)?, c"", nix::kmod::ModuleInitFlags::empty())`, tolerating `Errno::EEXIST` (already loaded → Ok). Add `"kmod"` to the nix feature list in the root `Cargo.toml`. Fake: record and return Ok. Run the new tests: PASS.

- [ ] **Step 6: imageboot module — failing tests** (`crates/machined/src/imageboot.rs` tests, using FakePlatform + FakeBlockBackend + tempdirs):

```rust
#[tokio::test]
async fn loads_modules_in_file_order_and_tolerates_missing_file() {
    let p = Arc::new(FakePlatform::new());
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("modules.load");
    std::fs::write(&list, "/lib/modules/v/a.ko\n/lib/modules/v/b.ko\n").unwrap();
    load_modules(p.as_ref(), &list).unwrap();
    assert_eq!(p.modules_loaded(), vec!["/lib/modules/v/a.ko", "/lib/modules/v/b.ko"]);
    // absent file = silent no-op (every non-image boot)
    load_modules(p.as_ref(), &dir.path().join("nope")).unwrap();
}

#[tokio::test]
async fn mounts_first_efi_labeled_partition_at_boot() {
    let p = Arc::new(FakePlatform::new());
    let b = FakeBlockBackend::new() /* seed: one volume with partition_label "EFI", device /dev/vda1, fs vfat */;
    mount_boot(&b, p.as_ref()).await.unwrap();
    // assert FakePlatform recorded mount of /dev/vda1 → /boot, fstype vfat
}

#[test]
fn seeds_pki_from_boot_when_state_pki_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (src, dst) = (dir.path().join("boot-pki"), dir.path().join("state-pki"));
    std::fs::create_dir_all(&src).unwrap();
    for f in ["ca.pem", "ca.key", "server.pem", "server.key"] {
        std::fs::write(src.join(f), f).unwrap();
    }
    seed_pki(&src, &dst).unwrap();
    for f in ["ca.pem", "ca.key", "server.pem", "server.key"] {
        assert!(dst.join(f).exists());
    }
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(dst.join("ca.key")).unwrap().permissions().mode() & 0o777, 0o600);
    // existing dst is never overwritten (PKI hygiene: no silent re-key/replace)
    std::fs::write(src.join("ca.pem"), "EVIL").unwrap();
    seed_pki(&src, &dst).unwrap();
    assert_eq!(std::fs::read_to_string(dst.join("ca.pem")).unwrap(), "ca.pem");
}

#[test]
fn config_path_prefers_boot_config_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let boot_cfg = dir.path().join("config.yaml");
    assert_eq!(pick_config_path(&boot_cfg, Path::new("/etc/machined/config.yaml")), PathBuf::from("/etc/machined/config.yaml"));
    std::fs::write(&boot_cfg, "machine: {}").unwrap();
    assert_eq!(pick_config_path(&boot_cfg, Path::new("/etc/machined/config.yaml")), boot_cfg);
}
```

- [ ] **Step 7: Implement imageboot.rs**

```rust
//! Early image-boot steps (pid1 only): load the imager-provided kernel module
//! list, mount the EFI boot partition, seed PKI from it, and prefer the boot
//! partition's machine config. Every step is a silent no-op when its input is
//! absent, so dev runs and tests are untouched.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use machined_block::BlockBackend;
use machined_platform::{MountSpec, Platform};
use tracing::{info, warn};

pub const MODULES_LOAD: &str = "/etc/machined/modules.load";
pub const BOOT_CONFIG: &str = "/boot/config.yaml";
pub const BOOT_PKI: &str = "/boot/pki";

/// Load every module listed (absolute .ko paths, dependency-ordered by the
/// imager). Missing list file = not an image boot = no-op.
pub fn load_modules(platform: &dyn Platform, list: &Path) -> anyhow::Result<()> {
    let Ok(text) = std::fs::read_to_string(list) else { return Ok(()) };
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Err(e) = platform.load_module(Path::new(line)) {
            warn!("loading module {line}: {e}"); // best-effort: a missing NIC module must not kill boot
        }
    }
    Ok(())
}

/// Mount the first GPT partition labeled EFI at /boot (vfat).
pub async fn mount_boot(block: &dyn BlockBackend, platform: &dyn Platform) -> anyhow::Result<()> {
    let vols = match block.list_volumes().await {
        Ok(v) => v,
        Err(e) => {
            info!("boot-partition scan skipped: {e}");
            return Ok(());
        }
    };
    let Some(efi) = vols.iter().find(|v| v.partition_label == "EFI") else {
        return Ok(());
    };
    if platform.is_mounted("/boot")? {
        return Ok(());
    }
    platform.mount(&MountSpec {
        source: efi.device.clone(),
        target: "/boot".into(),
        fstype: "vfat".into(),
        flags: 0,
        data: None,
    })?;
    info!("mounted boot partition {} at /boot", efi.device);
    Ok(())
}

/// Copy a pre-baked PKI from the boot partition to the runtime PKI dir,
/// enforcing 0700/0600 (FAT carries no unix perms). NEVER overwrites an
/// existing PKI dir — same no-silent-re-key posture as PkiError::Partial.
pub fn seed_pki(src: &Path, dst: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !src.join("ca.pem").exists() || dst.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o700))?;
    for f in ["ca.pem", "ca.key", "server.pem", "server.key"] {
        std::fs::copy(src.join(f), dst.join(f))?;
        let mode = if f.ends_with(".key") { 0o600 } else { 0o644 };
        std::fs::set_permissions(dst.join(f), std::fs::Permissions::from_mode(mode))?;
    }
    info!("seeded PKI from {}", src.display());
    Ok(())
}

/// The boot partition's config wins when it exists.
pub fn pick_config_path(boot: &Path, fallback: &Path) -> PathBuf {
    if boot.exists() { boot.to_path_buf() } else { fallback.to_path_buf() }
}
```

(Adjust `Arc` import if unused; match FakeBlockBackend's seeding API for the mount_boot test — see `crates/block/src/fake.rs`.)

- [ ] **Step 8: Wire into run_daemon + bind 0.0.0.0** (`crates/machined/src/main.rs`):

At the very top of `run_daemon`, after `build_platform()` and `spawn_reaper`, ADD (pid1-gated so dev runs never scan the host's disks):

```rust
// Image boot (pid1 only): essential mounts first so /sys exists for the
// block scan, then modules → /boot → PKI seed; all no-ops off-image.
if std::process::id() == 1 {
    if let Err(e) = platform.mount_essential() {
        error!("early mounts: {e}");
    }
    if let Err(e) = imageboot::load_modules(platform.as_ref(), Path::new(imageboot::MODULES_LOAD)) {
        error!("module load: {e}");
    }
    if let Err(e) = imageboot::mount_boot(build_block_backend_for_discovery().as_ref(), platform.as_ref()).await {
        error!("boot partition mount: {e}");
    }
    if let Err(e) = imageboot::seed_pki(Path::new(imageboot::BOOT_PKI), Path::new("/system/state/pki")) {
        error!("pki seed: {e}");
    }
}
```

Change the config load to `let config_path = imageboot::pick_config_path(Path::new(imageboot::BOOT_CONFIG), Path::new(DEFAULT_CONFIG_PATH));`. Change the API bind from `"127.0.0.1:50000"` to `"0.0.0.0:50000"` (mTLS authenticates every connection; QEMU hostfwd targets the guest NIC IP). `mount_essential` staying in the boot sequence is now harmless (idempotent). Add `mod imageboot;`.

- [ ] **Step 9: Gates + commit**

Run: `cargo test -p machined-pki -p machined-platform -p machined && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all`
Expected: all green, including the new imageboot + platform tests.

```bash
git add crates/pki crates/platform crates/machined crates/imager/src/pki.rs crates/imager/src/main.rs Cargo.toml
git commit -m "feat(machined): early image-boot steps (modules, /boot, PKI seed, boot config); bind API 0.0.0.0; gen-pki"
```

---

### Task 9: Provisioner CompleteLayout

A freshly-flashed image disk shows exactly one labeled partition: `EFI`. Add a fourth guarded decision that appends STATE+EPHEMERAL into free space (EPHEMERAL sized to the real disk) and formats ONLY the new partitions. EFI is never formatted, never re-partitioned — pin with tests.

**Files:**
- Modify: `crates/controllers/src/block/provision.rs`, `crates/block/src/lib.rs` (trait), `crates/block/src/sysfs.rs`, `crates/block/src/fake.rs`

- [ ] **Step 1: Failing guard tests** (provision.rs `guard_tests`):

```rust
#[test]
fn efi_only_disk_completes_layout() {
    // A flashed machined image: exactly one partition, labeled EFI.
    let d = vec![vol("sda", "EFI")];
    assert_eq!(plan_provisioning("/dev/sda", false, &d), ProvisionDecision::CompleteLayout);
}

#[test]
fn efi_plus_foreign_refuses() {
    let d = vec![vol("sda", "EFI"), vol("sda", "WINDOWS")];
    assert_eq!(plan_provisioning("/dev/sda", false, &d), ProvisionDecision::RefuseForeign);
}

#[test]
fn two_efi_partitions_refuse() {
    // Ambiguous: not the image layout, not ours — refuse.
    let d = vec![vol("sda", "EFI"), vol("sda", "EFI")];
    assert_eq!(plan_provisioning("/dev/sda", false, &d), ProvisionDecision::RefuseForeign);
}

#[test]
fn efi_only_with_wipe_still_provisions_fresh() {
    // Explicit wipe outranks adoption: operator asked for a clean slate.
    let d = vec![vol("sda", "EFI")];
    assert_eq!(plan_provisioning("/dev/sda", true, &d), ProvisionDecision::Provision);
}
```

- [ ] **Step 2: Guard implementation** — extend the enum + decision:

```rust
pub enum ProvisionDecision {
    Skip,
    Provision,
    /// The disk carries EXACTLY one partition, labeled EFI — a freshly
    /// flashed machined image. Append STATE+EPHEMERAL; never touch EFI.
    CompleteLayout,
    RefuseForeign,
}
```

In `plan_provisioning`, after `is_ours`:

```rust
let is_image = on_disk.len() == 1 && labels == ["EFI"];
if is_ours {
    ProvisionDecision::Skip
} else if wipe {
    ProvisionDecision::Provision
} else if is_image {
    ProvisionDecision::CompleteLayout
} else {
    ProvisionDecision::RefuseForeign
}
```

(wipe-before-image check order matters: explicit wipe wins — the test above pins it.)

- [ ] **Step 3: BlockProvisioner trait — failing fake test** (`crates/block/src/fake.rs` tests): `add_partitions` records `(disk, labels)` into the existing op-recording style and returns synthesized partition device paths continuing AFTER existing partitions (`/dev/sda2`, `/dev/sda3` when one exists). Then implement:

In `crates/block/src/lib.rs`, on `BlockProvisioner`:

```rust
/// Append `plan` to the disk's EXISTING GPT (no wipe, existing entries
/// untouched). Returns the new partitions' device paths, in plan order.
async fn add_partitions(&self, disk: &str, plan: &[PartitionPlan]) -> Result<Vec<String>>;
```

`SysfsBlock` impl: same as `create_partitions` (sysfs.rs:243) but (a) `.initialized(true)` open of the existing table, (b) NO `update_partitions(BTreeMap::new())` clear, (c) returned device paths numbered from `existing_count+1` (read `gdisk.partitions().len()` before adding). The size-0-last invariant carries over unchanged. Fake impl per its test.

- [ ] **Step 4: Failing controller tests** (provision.rs `controller_tests`):

```rust
#[tokio::test]
async fn image_disk_completes_layout_without_touching_efi() {
    let backend = Arc::new(FakeBlockBackend::new());
    let state = State::new();
    seed_disk_status(&state, "sda");
    seed_discovered(&state, "sda", "EFI");
    let ctx = ReconcileCtx { state: state.clone() };
    let mut c = VolumeProvisionerController::new(backend.clone(), provider("/dev/sda", false));
    c.reconcile(&ctx).await.unwrap();

    // No wipe, no full re-create; exactly one append.
    assert!(backend.wipes().is_empty());
    assert!(backend.creates().is_empty());
    assert_eq!(backend.adds().len(), 1);
    // EFI-NEVER: only the two new partitions get formatted.
    let formatted: Vec<String> = backend.formats(); // adapt to fake's record shape
    assert_eq!(formatted.len(), 2);
    assert!(formatted.iter().all(|f| !f.contains("EFI")), "{formatted:?}");
    // All three volumes published.
    assert_eq!(state.list(NS, ResourceType::VolumeStatus).len(), 3);
}
```

- [ ] **Step 5: Controller arm**

```rust
ProvisionDecision::CompleteLayout => {
    info!(disk = %disk, "image disk: completing layout (appending STATE+EPHEMERAL)");
    let layout: Vec<PartitionPlan> =
        fixed_layout().into_iter().filter(|p| p.label != "EFI").collect();
    let devices = self.backend.add_partitions(&disk, &layout).await.map_err(ctl)?;
    let mut statuses = provisioned_status_from_discovered(&disk, &discovered); // the EFI volume
    for (plan, device) in layout.iter().zip(devices.iter()) {
        self.backend.format(device, plan.fs, &plan.label).await.map_err(ctl)?;
        statuses.push(volume_status_obj(&plan.label, device, plan.fs.as_str(), &plan.label, VolumePhase::Provisioned));
    }
    reconcile_owned(&ctx.state, OWNER, NS, ResourceType::VolumeStatus, statuses)?;
}
```

- [ ] **Step 6: Gates + commit**

Run: `cargo test -p machined-block -p machined-controllers && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all`

```bash
git add crates/block crates/controllers
git commit -m "feat(block): CompleteLayout — adopt freshly-flashed image disks (append-only, EFI-never)"
```

---

### Task 10: Build-subcommand assembly

Wire Tasks 2-8 into `Command::Build`: manifest → fetch+extract apks → find kver + modules.dep → resolve closure → build initramfs → stage FAT tree (kernel from extracted apk's `boot/vmlinuz-virt`, initramfs, validated config.yaml, optional pki/) → write image → optional --emit-boot copy.

**Files:**
- Create: `crates/imager/src/build.rs`; modify `src/main.rs`

- [ ] **Step 1: Failing test** — end-to-end with synthetic artifacts (no network): construct a fake "linux-virt" apk (kernel file at `boot/vmlinuz-virt`, `lib/modules/6.12.81-0-virt/modules.dep` with the Task 4 fixture content + matching empty `.ko.gz` files) and a fake "e2fsprogs" apk (a `sbin/mkfs.ext4` file), a manifest pointing at `file://`-style paths — simplest: the test uses the `Fetch` trait with a map-backed fetcher. Assert: image exists, GPT has EFI, FAT contains `vmlinuz`, `initramfs.img`, `config.yaml`, `pki/ca.pem` when pki_dir given; `--emit-boot` dir has `vmlinuz` + `initramfs.img`; an INVALID config.yaml aborts the build before any image is written.

```rust
#[test]
fn invalid_config_aborts_before_image_write() {
    // build_image(...) with config text "machine: {bogus_field: 1}" must Err
    // (deny_unknown_fields) and out path must not exist afterwards.
}
```

(Write the full happy-path test per the description above — reuse `synthetic_apk`-style builders from Task 3's tests via a shared `#[cfg(test)] mod testutil` in the crate if duplication grows.)

- [ ] **Step 2: Implement build.rs**

```rust
//! The build pipeline: pinned artifacts → rootfs → initramfs → GPT/FAT image.

use crate::{apk, fetch::Fetch, image, initramfs, manifest::Manifest, modules};
use std::path::{Path, PathBuf};

pub struct BuildOpts<'a> {
    pub arch: &'a str,
    pub machined: &'a Path,
    pub config: &'a Path,
    pub out: &'a Path,
    pub size: u64,
    pub pki_dir: Option<&'a Path>,
    pub emit_boot: Option<&'a Path>,
    pub manifest: &'a Path,
    pub cache: &'a Path,
}

pub fn build(fetcher: &dyn Fetch, o: &BuildOpts) -> anyhow::Result<()> {
    // 1. Config must parse before anything is built.
    let config_text = std::fs::read_to_string(o.config)?;
    machined_config::load_from_str(&config_text)
        .map_err(|e| anyhow::anyhow!("config {} invalid: {e}", o.config.display()))?;

    // 2. Fetch + extract every pinned apk into a scratch rootfs.
    let m = Manifest::load(o.manifest)?;
    let arts = m.for_arch(o.arch).ok_or_else(|| anyhow::anyhow!("no artifacts for {}", o.arch))?;
    anyhow::ensure!(!arts.is_empty(), "artifact list for {} is empty — pin artifacts.toml first", o.arch);
    let scratch = tempfile::tempdir()?;
    let rootfs = scratch.path().join("rootfs");
    for a in arts {
        let path = crate::fetch::fetch_verified(fetcher, &a.url, &a.sha256, o.cache)?;
        match a.kind.as_str() {
            "apk" => apk::extract_apk(&path, &rootfs)?,
            k => anyhow::bail!("unknown artifact kind {k}"),
        }
    }

    // 3. Kernel + module closure from the extracted tree.
    let kver = find_kver(&rootfs)?; // the single dir under lib/modules/
    let dep = std::fs::read_to_string(rootfs.join("lib/modules").join(&kver).join("modules.dep"))?;
    let mods = modules::resolve_closure(&dep, modules::X86_64_QEMU_MODULES)?;
    let kernel = rootfs.join("boot/vmlinuz-virt");
    anyhow::ensure!(kernel.exists(), "kernel missing from linux-virt apk");
    let kernel_bytes = std::fs::read(&kernel)?;

    // 4. Initramfs: prune boot/ + unneeded modules from the rootfs copy first
    //    (the initramfs carries ONLY the resolved closure, not all ~100MB).
    prune_for_initramfs(&rootfs, &kver, &mods)?;
    let initrd = initramfs::build_initramfs(&rootfs, o.machined, &mods, &kver)?;

    // 5. Stage the FAT tree and write the image.
    let staging = scratch.path().join("staging");
    std::fs::create_dir_all(&staging)?;
    std::fs::write(staging.join("vmlinuz"), &kernel_bytes)?;
    std::fs::write(staging.join("initramfs.img"), &initrd)?;
    std::fs::write(staging.join("config.yaml"), &config_text)?;
    if let Some(pki) = o.pki_dir {
        let dst = staging.join("pki");
        std::fs::create_dir_all(&dst)?;
        for f in ["ca.pem", "ca.key", "server.pem", "server.key"] {
            std::fs::copy(pki.join(f), dst.join(f))?;
        }
    }
    image::write_image(o.out, o.size, &staging)?;
    if let Some(dir) = o.emit_boot {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("vmlinuz"), &kernel_bytes)?;
        std::fs::write(dir.join("initramfs.img"), &initrd)?;
    }
    println!("image written to {}", o.out.display());
    Ok(())
}
```

`find_kver`: read_dir `lib/modules`, expect exactly one entry, return its name (error otherwise). `prune_for_initramfs`: delete `rootfs/boot` and every file under `lib/modules/<kver>` whose relative path is not in the closure set (keep `modules.dep` out too — machined doesn't need it). Wire `Command::Build` in main.rs to call `build(&HttpFetcher, &opts)`.

- [ ] **Step 3: Gates + commit**

Run: `cargo test -p machined-imager && cargo clippy -p machined-imager --all-targets -- -D warnings && cargo fmt --all`

```bash
git add crates/imager
git commit -m "feat(imager): build subcommand — full pipeline from pinned apks to bootable image"
```

---

### Task 11: Pin real artifacts + CI node config + musl build

**Files:**
- Modify: `crates/imager/artifacts.toml`
- Create: `examples/node-ci.yaml`
- Modify: `Makefile`

- [ ] **Step 1: Discover current Alpine v3.21 package versions and pin**

```bash
for p in linux-virt musl e2fsprogs e2fsprogs-libs libblkid libuuid; do
  curl -sL "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/" | grep -oE "${p}-[0-9][^\"]*\.apk" | sort -u | tail -1
done
```

Then for each chosen file: `curl -sL <url> | sha256sum`. Fill `artifacts.toml`:

```toml
[artifact]

[[artifact.x86_64]]
name = "linux-virt"
url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/linux-virt-<VER>.apk"
sha256 = "<SHA>"
kind = "apk"

# musl loader + shared libs for the (dynamically linked) e2fsprogs mkfs.ext4
[[artifact.x86_64]]
name = "musl"
url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/musl-<VER>.apk"
sha256 = "<SHA>"
kind = "apk"

[[artifact.x86_64]]
name = "e2fsprogs"
url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/e2fsprogs-<VER>.apk"
sha256 = "<SHA>"
kind = "apk"

[[artifact.x86_64]]
name = "e2fsprogs-libs"
url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/e2fsprogs-libs-<VER>.apk"
sha256 = "<SHA>"
kind = "apk"

[[artifact.x86_64]]
name = "libblkid"
url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/libblkid-<VER>.apk"
sha256 = "<SHA>"
kind = "apk"

[[artifact.x86_64]]
name = "libuuid"
url = "https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/libuuid-<VER>.apk"
sha256 = "<SHA>"
kind = "apk"
```

Verify mkfs.ext4's runtime deps are fully covered: extract the e2fsprogs apk locally (`tar -tzvf`) and check `ldd`-style NEEDED entries (`readelf -d sbin/mkfs.ext4 | grep NEEDED`) — every `lib*.so.*` must come from one of the pinned apks; add any missing package the same way. Also check whether `sbin/mkfs.ext4` is a symlink to `mke2fs` — if so, implement the symlink support described in Task 6's NOTE.

- [ ] **Step 2: CI node config** — `examples/node-ci.yaml`:

```yaml
# QEMU user-net (slirp) static layout: guest 10.0.2.15/24, gw 10.0.2.2, dns 10.0.2.3.
machine:
  hostname: node-ci
  network:
    interfaces:
      - name: eth0
        addresses: ["10.0.2.15/24"]
        routes:
          - via: 10.0.2.2
    nameservers: [10.0.2.3]
  install:
    disk: /dev/vda
    wipe: false        # the image flow NEVER wipes; CompleteLayout adopts it
  runtime:
    disabled: true     # no containerd until M7b
```

Validate locally: `cargo run -p machined-imager -- build --arch x86_64 ... --config examples/node-ci.yaml ...` parses it (or a one-line unit test in config crate is unnecessary — `build()` already validates).

- [ ] **Step 3: musl target + Makefile**

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt-get install -y musl-tools   # cc for ring's C bits; document, run locally
```

Makefile additions:

```makefile
# Static machined for images (vendored protoc; needs musl-tools).
dist-x86_64:
	cargo build --release --target x86_64-unknown-linux-musl -p machined

# Build the x86_64 image + boot it in QEMU, assert the node comes up.
boot-test: dist-x86_64
	cargo build --release -p machined-imager -p machinectl
	./scripts/boot-test-x86_64.sh
```

Run `make dist-x86_64` — expect a successful static build (`file target/x86_64-unknown-linux-musl/release/machined` → "statically linked"). If `ring`/`protoc` trip on musl, fix per error (typically just musl-tools; CC=musl-gcc env if needed).

- [ ] **Step 4: Commit**

```bash
git add crates/imager/artifacts.toml examples/node-ci.yaml Makefile
git commit -m "feat(imager): pin Alpine v3.21 artifacts; CI node config; musl dist target"
```

---

### Task 12: Boot-test script

**Files:**
- Create: `scripts/boot-test-x86_64.sh` (chmod +x)

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# Boot the x86_64 image in QEMU and assert the node comes up:
# mTLS API answers; STATE+EPHEMERAL provisioned (CompleteLayout ran).
set -euo pipefail
cd "$(dirname "$0")/.."

WORK=target/boot-test
IMG=$WORK/machined-x86_64.img
SERIAL=$WORK/serial.log
MACHINED=target/x86_64-unknown-linux-musl/release/machined
IMAGER=target/release/machined-imager
CTL=target/release/machinectl
TIMEOUT=${BOOT_TEST_TIMEOUT:-150}

rm -rf "$WORK"; mkdir -p "$WORK"

"$IMAGER" gen-pki --out "$WORK/pki"
"$IMAGER" build --arch x86_64 --machined "$MACHINED" \
  --config examples/node-ci.yaml --pki-dir "$WORK/pki" \
  --out "$IMG" --emit-boot "$WORK/boot"

KVM_FLAG=""
[ -w /dev/kvm ] && KVM_FLAG="-enable-kvm -cpu host"

qemu-system-x86_64 $KVM_FLAG -m 512 -machine q35 \
  -kernel "$WORK/boot/vmlinuz" -initrd "$WORK/boot/initramfs.img" \
  -append "console=ttyS0" \
  -drive file="$IMG",if=virtio,format=raw \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:50000-:50000 \
  -device virtio-net-pci,netdev=n0 \
  -display none -serial "file:$SERIAL" &
QEMU=$!
trap 'kill $QEMU 2>/dev/null || true' EXIT

ctl() { "$CTL" --bundle "$WORK/pki/machinectl" --endpoint https://127.0.0.1:50000 "$@"; }

echo "waiting for API (max ${TIMEOUT}s)..."
for i in $(seq "$TIMEOUT"); do
  if ctl version >/dev/null 2>&1; then break; fi
  if ! kill -0 $QEMU 2>/dev/null; then echo "QEMU died"; tail -50 "$SERIAL"; exit 1; fi
  [ "$i" = "$TIMEOUT" ] && { echo "TIMEOUT waiting for API"; tail -80 "$SERIAL"; exit 1; }
  sleep 1
done
echo "API up: $(ctl version)"

echo "checking provisioned volumes..."
for i in $(seq 60); do
  VOLS=$(ctl get VolumeStatus --namespace block 2>/dev/null || true)
  if echo "$VOLS" | grep -q STATE && echo "$VOLS" | grep -q EPHEMERAL; then
    echo "$VOLS"; echo "BOOT TEST PASSED"; exit 0
  fi
  sleep 2
done
echo "volumes never provisioned:"; ctl get VolumeStatus --namespace block || true
tail -80 "$SERIAL"; exit 1
```

Check the actual `machinectl get` output column for the volume rows and adjust the greps (run `machinectl get VolumeStatus --namespace block` against the fake-backed dev flow or read `crates/machinectl/src/main.rs`'s print format).

- [ ] **Step 2: Run locally**

Run: `make boot-test`
Expected: "BOOT TEST PASSED" within ~60s (KVM) — this is the milestone's moment of truth. Debug via `target/boot-test/serial.log` (machined's tracing goes to the serial console). Common failure modes, in boot order: kernel panic "no init" (initramfs malformed → check cpio test against `cpio -it < initramfs`), silent hang (missing /dev/console node), "Failed to execute /init" (machined not actually static — re-check musl build), module load errors in log, no /dev/vda (virtio modules), API connection refused (network config / 0.0.0.0 bind / hostfwd), TLS failure (PKI seed path).

- [ ] **Step 3: Commit**

```bash
git add scripts/boot-test-x86_64.sh
git commit -m "test(boot): QEMU x86_64 boot test — API over mTLS + CompleteLayout provisioning"
```

---

### Task 13: CI job

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add the boot-test job**

```yaml
  boot-test:
    runs-on: ubuntu-latest
    needs: check
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: x86_64-unknown-linux-musl
      - uses: Swatinem/rust-cache@v2
      - name: install qemu + musl
        run: |
          sudo apt-get update
          sudo apt-get install -y --no-install-recommends qemu-system-x86 musl-tools
          # KVM is enabled on GitHub-hosted Linux runners; make it accessible.
          echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules
          sudo udevadm control --reload-rules
          sudo udevadm trigger --name-match=kvm
      - name: cache imager artifacts
        uses: actions/cache@v4
        with:
          path: target/imager-cache
          key: imager-artifacts-${{ hashFiles('crates/imager/artifacts.toml') }}
      - name: boot test
        run: make boot-test
      - name: upload serial log
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: boot-test-serial-log
          path: target/boot-test/serial.log
          if-no-files-found: ignore
```

- [ ] **Step 2: Push the branch, watch the job**

Run: `git add .github/workflows/ci.yml && git commit -m "ci: QEMU boot test job (KVM, cached artifacts, serial log on failure)" && git push -u origin <branch>`
Then: `gh run watch` (or poll `gh run list`). Expected: boot-test green. If KVM is unavailable on the runner, the script's `[ -w /dev/kvm ]` guard falls back to TCG — slower but typically still under the timeout for this tiny boot; bump `BOOT_TEST_TIMEOUT` via env in the workflow step if needed.

---

### Task 14: Docs + finish

**Files:**
- Modify: `README.md`, `docs/superpowers/specs/2026-06-12-machined-rs-m7-image-pipeline-design.md` (status note only if anything diverged)

- [ ] **Step 1: README status table** — flip `Bootable image pipeline` to `✅ x86_64 (QEMU); ARM 🔜 M7c`, and update the "no installer image yet" paragraph: there IS an image now; bare-metal x86 + Pi still pending. Add a one-liner under Build & test: `make boot-test  # build image + boot it in QEMU, assert API+provisioning`.

- [ ] **Step 2: Gates, then finishing**

Run: `make pre-commit`
Expected: clean. Then follow superpowers:finishing-a-development-branch (merge --no-ff to main per project convention, delete branch, push, verify CI on main).

---

## Verification (end-to-end)

1. `cargo test --workspace` — all unit/integration tests green (imager: manifest/fetch/apk/modules/cpio/initramfs/image/build; machined: imageboot; block+controllers: CompleteLayout).
2. `make boot-test` locally — full pipeline: pinned downloads → image → QEMU boot → mTLS `machinectl version` → `VolumeStatus` shows STATE+EPHEMERAL `Provisioned`. THE milestone gate.
3. CI: `check` job green + `boot-test` job green on the PR branch and on main after merge.
4. Safety pins intact: `cargo test -p machined-controllers` includes EFI-never formatting assert + wipe-precedence + two-EFI-refuse.

## Known gaps (deliberate, documented)

- PKI persists only via the FAT seed (PKI setup runs before STATE is mounted, so unseeded image boots re-key each boot). Fix lands with M7b (delay PKI/API start until after mounts) — README/spec note it.
- `ca.key` on the FAT partition is readable by anyone with the SD card/image — acceptable for CI; operators are warned in the imager's --pki-dir help text.
- x86_64 bare-metal self-boot, aarch64/Pi: M7c per spec.
- Flash-to-larger-disk leaves the backup GPT mid-disk (image is 4 GiB sparse); kernel reads the primary fine, `add_partitions` on the open existing table may warn. Handled for real in M7c (Pi SD cards) — backup-header relocation on first boot.
- PKI-init races the STATE mount. The early `seed_pki` (Task 8) copies the baked PKI to `/system/state/pki` BEFORE the STATE volume is mounted there, so the seed lands on the initramfs rootfs and is shadowed once STATE mounts over it. On a cold boot this is benign (PKI init also runs pre-mount and sees the seeded files). On a warm boot, a fast STATE mount can win the race and present an empty `/system/state/pki` to PKI init, which then mints a fresh CA — permanently locking out the image's baked machinectl client bundle. The real fix is M7b's reordering (PKI/API init after the STATE mount). Until M7b lands, CI must remain a single cold boot per image; do not add warm-reboot assertions that talk to the API with the baked bundle.

## Outcome

Boot test green on CI: the image boots in QEMU, the mTLS API answers (`API up: 0.1.0`), and `VolumeStatus` shows EFI vfat + STATE/EPHEMERAL ext4 all `Provisioned`. Run time ~3.5 min warm.

The real boot shook out four bugs that unit tests against fakes could never have caught — which is exactly the boot-test's reason to exist:

1. **vfat mount failed without `nls_utf8`** — mounting the EFI partition needed an `iocharset` the initramfs didn't carry; the module closure had to include the NLS codec.
2. **PID 1 ran with an empty `PATH`** — the kernel hands `/init` no environment, so musl's `execvp` of helpers like `mkfs.ext4` in `/sbin` failed until machined set `PATH` itself.
3. **`address EEXIST` wasn't treated as converged** — re-adding an address already present returned `EEXIST` and stalled the network reconcile instead of being read as already-applied.
4. **route controller wasn't watching `AddressStatus`** — routes reconciled before their address existed and never re-ran, so the default route never installed until the input edge was added.
