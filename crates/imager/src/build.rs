//! The build pipeline: pinned artifacts → rootfs → initramfs → GPT/FAT image.

use crate::{apk, fetch::Fetch, image, initramfs, manifest::Manifest, modules};
use anyhow::Context as _;
use std::path::Path;

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

/// The four server PKI files copied onto the boot partition.
const PKI_FILES: [&str; 4] = ["ca.pem", "ca.key", "server.pem", "server.key"];

/// Run the full image build: validate the config, fetch + extract every pinned
/// apk, resolve the kernel module closure, assemble the initramfs, and write a
/// GPT/FAT image (optionally copying a kernel/initramfs pair to `emit_boot`).
///
/// Nothing is written until the config parses, so an invalid config aborts
/// before any output file exists.
///
/// # Errors
///
/// Returns an error if the config is missing or invalid, the manifest is
/// missing/malformed or has no (or an empty) artifact list for `arch`, any
/// artifact fails to fetch/verify/extract, the extracted tree is missing the
/// kernel or a unique module version, the module closure cannot be resolved,
/// the initramfs cannot be built, a requested PKI file is missing, or any image
/// I/O fails.
pub fn build(fetcher: &dyn Fetch, o: &BuildOpts) -> anyhow::Result<()> {
    // 1. Config must parse before anything is built.
    let config_text = std::fs::read_to_string(o.config)
        .with_context(|| format!("reading config {}", o.config.display()))?;
    machined_config::load_from_str(&config_text)
        .map_err(|e| anyhow::anyhow!("config {} invalid: {e}", o.config.display()))?;

    // PKI files are operator-assembled by hand; check all four up front so a
    // missing one fails loudly before any expensive work, not mid-copy.
    if let Some(pki) = o.pki_dir {
        for f in PKI_FILES {
            let p = pki.join(f);
            anyhow::ensure!(p.exists(), "PKI file missing: {}", p.display());
        }
    }

    // 2. Fetch + extract every pinned apk into a scratch rootfs.
    let m = Manifest::load(o.manifest)?;
    let arts = m
        .for_arch(o.arch)
        .ok_or_else(|| anyhow::anyhow!("no artifacts for {}", o.arch))?;
    anyhow::ensure!(
        !arts.is_empty(),
        "artifact list for {} is empty — pin artifacts.toml first",
        o.arch
    );
    let scratch = tempfile::tempdir().context("creating scratch dir")?;
    let rootfs = scratch.path().join("rootfs");
    let staging = scratch.path().join("staging");
    let staging_bin = staging.join("bin");
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("creating staging dir {}", staging.display()))?;
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

    // 3. Kernel + module closure from the extracted tree.
    let kver = find_kver(&rootfs)?;
    let dep_path = rootfs.join("lib/modules").join(&kver).join("modules.dep");
    let dep = std::fs::read_to_string(&dep_path)
        .with_context(|| format!("reading {}", dep_path.display()))?;
    let cfg = crate::arch::arch_config(o.arch)
        .ok_or_else(|| anyhow::anyhow!("unknown arch {}", o.arch))?;
    let mods = modules::resolve_closure(&dep, cfg.module_roots)?;
    let kernel = rootfs.join(cfg.kernel_path);
    anyhow::ensure!(
        kernel.exists(),
        "kernel {} missing from apk",
        cfg.kernel_path
    );
    let kernel_bytes =
        std::fs::read(&kernel).with_context(|| format!("reading kernel {}", kernel.display()))?;

    // 4. Initramfs carries ONLY the resolved closure, not all modules.
    prune_for_initramfs(&rootfs, &kver, &mods)?;
    let initrd = initramfs::build_initramfs(&rootfs, o.machined, &mods, &kver)?;

    // 5. Stage the FAT tree and write the image. (staging/ and staging/bin were
    // created before the fetch loop so boot artifacts could land in /boot/bin.)
    let vmlinuz = staging.join("vmlinuz");
    std::fs::write(&vmlinuz, &kernel_bytes)
        .with_context(|| format!("writing {}", vmlinuz.display()))?;
    let initramfs_img = staging.join("initramfs.img");
    std::fs::write(&initramfs_img, &initrd)
        .with_context(|| format!("writing {}", initramfs_img.display()))?;
    let config_dst = staging.join("config.yaml");
    std::fs::write(&config_dst, &config_text)
        .with_context(|| format!("writing {}", config_dst.display()))?;
    if let Some(pki) = o.pki_dir {
        let dst = staging.join("pki");
        std::fs::create_dir_all(&dst).with_context(|| format!("creating {}", dst.display()))?;
        for f in PKI_FILES {
            std::fs::copy(pki.join(f), dst.join(f))
                .with_context(|| format!("copying PKI file {f}"))?;
        }
    }
    image::write_image(o.out, o.size, &staging, cfg.scheme)?;
    if let Some(dir) = o.emit_boot {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating emit-boot dir {}", dir.display()))?;
        let boot_vmlinuz = dir.join("vmlinuz");
        std::fs::write(&boot_vmlinuz, &kernel_bytes)
            .with_context(|| format!("writing {}", boot_vmlinuz.display()))?;
        let boot_initrd = dir.join("initramfs.img");
        std::fs::write(&boot_initrd, &initrd)
            .with_context(|| format!("writing {}", boot_initrd.display()))?;
    }
    println!("image written to {}", o.out.display());
    Ok(())
}

/// Find the single kernel version under `rootfs/lib/modules`.
///
/// # Errors
///
/// Fails if the directory cannot be read, is empty, or holds more than one
/// entry (a single linux-virt apk ships exactly one).
fn find_kver(rootfs: &Path) -> anyhow::Result<String> {
    let modules_dir = rootfs.join("lib/modules");
    let mut names: Vec<String> = std::fs::read_dir(&modules_dir)
        .with_context(|| format!("reading {}", modules_dir.display()))?
        .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
        .collect::<anyhow::Result<_>>()?;
    names.sort();
    match names.as_slice() {
        [one] => Ok(one.clone()),
        other => anyhow::bail!(
            "expected exactly one kernel version under {}, found {:?}",
            modules_dir.display(),
            other
        ),
    }
}

/// Strip the rootfs down to what the initramfs needs: drop `/boot` entirely
/// (the kernel travels outside the cpio) and, under `lib/modules/<kver>`, keep
/// only the files in the resolved closure (modules.dep included goes — machined
/// loads the precomputed `modules.load` and needs no dep file). Now-empty
/// directories are removed.
///
/// # Errors
///
/// Returns an error on any filesystem read/remove failure.
fn prune_for_initramfs(rootfs: &Path, kver: &str, mods: &[String]) -> anyhow::Result<()> {
    let boot = rootfs.join("boot");
    if boot.exists() {
        std::fs::remove_dir_all(&boot).with_context(|| format!("removing {}", boot.display()))?;
    }

    let mod_root = rootfs.join("lib/modules").join(kver);
    let keep: std::collections::BTreeSet<&str> = mods.iter().map(String::as_str).collect();
    prune_modules(&mod_root, &mod_root, &keep)?;
    Ok(())
}

/// Post-order walk of `dir`: delete every regular file whose path relative to
/// `mod_root` is not in `keep`, then remove directories left empty.
fn prune_modules(
    mod_root: &Path,
    dir: &Path,
    keep: &std::collections::BTreeSet<&str>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            prune_modules(mod_root, &path, keep)?;
            // Drop the dir if the recursion emptied it.
            if std::fs::read_dir(&path)?.next().is_none() {
                std::fs::remove_dir(&path)
                    .with_context(|| format!("removing empty dir {}", path.display()))?;
            }
        } else {
            let rel = path.strip_prefix(mod_root)?.to_string_lossy().into_owned();
            if !keep.contains(rel.as_str()) {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::path::PathBuf;

    /// A map-backed fetcher: url → bytes, no network.
    struct MapFetcher(BTreeMap<String, Vec<u8>>);
    impl Fetch for MapFetcher {
        fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
            self.0
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no such url {url}"))
        }
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    const KVER: &str = "6.12.81-0-virt";

    /// modules.dep for the QEMU closure: every VIRT_MODULES root and its
    /// transitive deps. nls_cp437/nls_iso8859_1/nls_utf8 are dep-less roots.
    const MODULES_DEP: &str = "\
kernel/drivers/block/virtio_blk.ko.gz: kernel/drivers/virtio/virtio.ko.gz
kernel/drivers/net/virtio_net.ko.gz: kernel/drivers/virtio/virtio.ko.gz
kernel/drivers/virtio/virtio.ko.gz:
kernel/fs/ext4/ext4.ko.gz: kernel/fs/jbd2/jbd2.ko.gz kernel/lib/crc16.ko.gz
kernel/fs/jbd2/jbd2.ko.gz:
kernel/lib/crc16.ko.gz:
kernel/fs/fat/vfat.ko.gz: kernel/fs/fat/fat.ko.gz
kernel/fs/fat/fat.ko.gz:
kernel/fs/nls/nls_cp437.ko.gz:
kernel/fs/nls/nls_iso8859_1.ko.gz:
kernel/fs/nls/nls_utf8.ko.gz:
";

    /// All `.ko.gz` paths declared in MODULES_DEP (closure members) plus one
    /// extra module NOT in any closure, to prove the initramfs prunes it.
    const ALL_KO_GZ: &[&str] = &[
        "kernel/drivers/block/virtio_blk.ko.gz",
        "kernel/drivers/net/virtio_net.ko.gz",
        "kernel/drivers/virtio/virtio.ko.gz",
        "kernel/fs/ext4/ext4.ko.gz",
        "kernel/fs/jbd2/jbd2.ko.gz",
        "kernel/lib/crc16.ko.gz",
        "kernel/fs/fat/vfat.ko.gz",
        "kernel/fs/fat/fat.ko.gz",
        "kernel/fs/nls/nls_cp437.ko.gz",
        "kernel/fs/nls/nls_iso8859_1.ko.gz",
        "kernel/fs/nls/nls_utf8.ko.gz",
    ];
    const UNUSED_KO_GZ: &str = "kernel/net/unused.ko.gz";

    fn append_file(b: &mut tar::Builder<&mut Vec<u8>>, name: &str, mode: u32, data: &[u8]) {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        b.append_data(&mut h, name, data).unwrap();
    }

    /// linux-virt apk: kernel, modules.dep, and a `.ko.gz` for every module
    /// (closure + one unused), each an empty gzip stream.
    fn linux_virt_apk() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            append_file(&mut b, ".PKGINFO", 0o644, b"meta");
            append_file(&mut b, "boot/vmlinuz-virt", 0o644, b"KERNEL");
            let dep_path = format!("lib/modules/{KVER}/modules.dep");
            append_file(&mut b, &dep_path, 0o644, MODULES_DEP.as_bytes());
            let empty_ko = gzip(b"");
            for ko in ALL_KO_GZ.iter().chain(std::iter::once(&UNUSED_KO_GZ)) {
                let path = format!("lib/modules/{KVER}/{ko}");
                append_file(&mut b, &path, 0o644, &empty_ko);
            }
            b.finish().unwrap();
        }
        gzip(&tar_bytes)
    }

    /// e2fsprogs apk: just `sbin/mkfs.ext4`.
    fn e2fsprogs_apk() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            append_file(&mut b, "sbin/mkfs.ext4", 0o755, b"\x7fELF!");
            b.finish().unwrap();
        }
        gzip(&tar_bytes)
    }

    /// containerd static tarball: `bin/containerd` (plus a non-bin entry that
    /// must be ignored), gzipped — the shape extract_boot_tarball expects.
    fn containerd_tgz() -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            append_file(&mut b, "bin/containerd", 0o755, b"ELF-c");
            append_file(&mut b, "LICENSE", 0o644, b"license");
            b.finish().unwrap();
        }
        gzip(&tar_bytes)
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        manifest: PathBuf,
        config: PathBuf,
        machined: PathBuf,
        cache: PathBuf,
        out: PathBuf,
        pki_dir: PathBuf,
        emit_boot: PathBuf,
        fetcher: MapFetcher,
    }

    /// Build a fixture: two fake apks pinned in a temp manifest with real
    /// sha256s, a valid config, a machined stub, and a generated PKI dir.
    fn fixture(config_text: &str, linux_sha_override: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();

        let linux = linux_virt_apk();
        let e2fs = e2fsprogs_apk();
        let containerd = containerd_tgz();
        let runc = b"ELF-runc".to_vec();
        let linux_sha = hex::encode(Sha256::digest(&linux));
        let e2fs_sha = hex::encode(Sha256::digest(&e2fs));
        let containerd_sha = hex::encode(Sha256::digest(&containerd));
        let runc_sha = hex::encode(Sha256::digest(&runc));

        let linux_url = "http://example/linux-virt.apk".to_string();
        let e2fs_url = "http://example/e2fsprogs.apk".to_string();
        let containerd_url = "http://example/containerd.tar.gz".to_string();
        let runc_url = "http://example/runc.amd64".to_string();
        let mut map = BTreeMap::new();
        map.insert(linux_url.clone(), linux);
        map.insert(e2fs_url.clone(), e2fs);
        map.insert(containerd_url.clone(), containerd);
        map.insert(runc_url.clone(), runc);

        let pinned_linux_sha = linux_sha_override.unwrap_or(&linux_sha);
        let manifest = base.join("artifacts.toml");
        std::fs::write(
            &manifest,
            format!(
                r#"
[[artifact.x86_64]]
name = "linux-virt"
url = "{linux_url}"
sha256 = "{pinned_linux_sha}"
kind = "apk"

[[artifact.x86_64]]
name = "e2fsprogs"
url = "{e2fs_url}"
sha256 = "{e2fs_sha}"
kind = "apk"

[[artifact.x86_64]]
name = "containerd"
url = "{containerd_url}"
sha256 = "{containerd_sha}"
kind = "boot-tarball"

[[artifact.x86_64]]
name = "runc"
url = "{runc_url}"
sha256 = "{runc_sha}"
kind = "boot-binary"
rename = "runc"
"#
            ),
        )
        .unwrap();

        let config = base.join("config.yaml");
        std::fs::write(&config, config_text).unwrap();
        let machined = base.join("machined");
        std::fs::write(&machined, b"machined-elf").unwrap();
        let pki_dir = base.join("pki");
        machined_pki::NodePki::load_or_generate(&pki_dir, "node", &["127.0.0.1".into()]).unwrap();

        Fixture {
            _dir: dir,
            manifest,
            config,
            machined,
            cache: base.join("cache"),
            out: base.join("out.img"),
            pki_dir,
            emit_boot: base.join("boot"),
            fetcher: MapFetcher(map),
        }
    }

    fn opts<'a>(f: &'a Fixture, pki: bool, emit: bool) -> BuildOpts<'a> {
        BuildOpts {
            arch: "x86_64",
            machined: &f.machined,
            config: &f.config,
            out: &f.out,
            size: 600 * 1024 * 1024,
            pki_dir: pki.then_some(f.pki_dir.as_path()),
            emit_boot: emit.then_some(f.emit_boot.as_path()),
            manifest: &f.manifest,
            cache: &f.cache,
        }
    }

    fn gunzip(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        GzDecoder::new(bytes).read_to_end(&mut out).unwrap();
        out
    }

    /// Read the FAT root of the built image; return (filesystem-less) helper.
    fn open_fat(
        img: &Path,
    ) -> (
        Vec<String>,
        fatfs::FileSystem<fscommon::StreamSlice<std::fs::File>>,
    ) {
        let disk = gpt::GptConfig::new().writable(false).open(img).unwrap();
        let parts = disk.partitions();
        assert_eq!(parts.len(), 1, "exactly one (EFI) partition");
        let p = parts.values().next().unwrap();
        assert_eq!(p.name, "EFI");
        assert_eq!(p.part_type_guid, gpt::partition_types::EFI);
        let (start, end) = (p.first_lba * 512, (p.last_lba + 1) * 512);
        let file = std::fs::File::options().read(true).open(img).unwrap();
        let slice = fscommon::StreamSlice::new(file, start, end).unwrap();
        let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
        let names: Vec<String> = fs
            .root_dir()
            .iter()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "." && n != "..")
            .collect();
        (names, fs)
    }

    fn read_fat_file(
        fs: &fatfs::FileSystem<fscommon::StreamSlice<std::fs::File>>,
        path: &str,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut dir = fs.root_dir();
        let (dirs, file) = path.rsplit_once('/').map_or((vec![], path), |(d, f)| {
            (d.split('/').collect::<Vec<_>>(), f)
        });
        for d in dirs {
            dir = dir.open_dir(d).unwrap();
        }
        dir.open_file(file).unwrap().read_to_end(&mut buf).unwrap();
        buf
    }

    #[test]
    fn happy_path_builds_image_with_all_boot_files() {
        let f = fixture("machine: {}\n", None);
        build(&f.fetcher, &opts(&f, true, true)).unwrap();

        assert!(f.out.exists(), "image must exist");
        let (names, fs) = open_fat(&f.out);
        for want in ["vmlinuz", "initramfs.img", "config.yaml", "pki"] {
            assert!(
                names.contains(&want.to_string()),
                "FAT root missing {want}: {names:?}"
            );
        }

        // vmlinuz content and config exact text.
        assert_eq!(read_fat_file(&fs, "vmlinuz"), b"KERNEL");
        assert_eq!(read_fat_file(&fs, "config.yaml"), b"machine: {}\n");

        // PKI: all four server files present.
        for f4 in PKI_FILES {
            let p = format!("pki/{f4}");
            assert!(!read_fat_file(&fs, &p).is_empty(), "pki file {f4} empty");
        }

        // Boot binaries: the containerd tarball's bin/* and the renamed runc
        // land in the FAT's /bin (boot-tarball + boot-binary kinds), with the
        // tarball's non-bin entries skipped.
        let bin = fs.root_dir().open_dir("bin").expect("bin dir on FAT");
        let bin_names: Vec<String> = bin
            .iter()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "." && n != "..")
            .collect();
        assert!(
            bin_names.contains(&"containerd".to_string()),
            "{bin_names:?}"
        );
        assert!(bin_names.contains(&"runc".to_string()), "{bin_names:?}");
        assert!(
            !bin_names.contains(&"LICENSE".to_string()),
            "non-bin tarball entries must be skipped: {bin_names:?}"
        );
        assert_eq!(read_fat_file(&fs, "bin/containerd"), b"ELF-c");
        assert_eq!(read_fat_file(&fs, "bin/runc"), b"ELF-runc");

        // initramfs.img gunzips to a cpio with the expected payload.
        let initrd = gunzip(&read_fat_file(&fs, "initramfs.img"));
        let cpio = String::from_utf8_lossy(&initrd);
        assert!(cpio.contains("init\0"), "init present");
        assert!(
            cpio.contains("etc/machined/modules.load"),
            "modules.load present"
        );
        assert!(
            cpio.contains("sbin/mkfs.ext4"),
            "mkfs.ext4 from e2fsprogs present"
        );
        // boot/ pruned: no kernel in the cpio.
        assert!(
            !cpio.contains("boot/vmlinuz-virt"),
            "kernel must be pruned from initramfs"
        );
        // The unused module must be absent; a closure module must be present.
        assert!(
            !cpio.contains("unused.ko"),
            "non-closure module must be pruned"
        );
        assert!(cpio.contains("ext4.ko"), "closure module ext4 present");

        // emit_boot mirrors the FAT copies exactly.
        assert_eq!(
            std::fs::read(f.emit_boot.join("vmlinuz")).unwrap(),
            read_fat_file(&fs, "vmlinuz")
        );
        assert_eq!(
            std::fs::read(f.emit_boot.join("initramfs.img")).unwrap(),
            read_fat_file(&fs, "initramfs.img")
        );
    }

    #[test]
    fn invalid_config_aborts_before_any_output() {
        let f = fixture("machine: {bogus_field: 1}\n", None);
        let err = build(&f.fetcher, &opts(&f, false, true)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&f.config.display().to_string()),
            "error must mention the config path: {msg}"
        );
        assert!(!f.out.exists(), "no image on invalid config");
        assert!(
            !f.emit_boot.exists() || std::fs::read_dir(&f.emit_boot).unwrap().next().is_none(),
            "emit_boot must be empty/absent"
        );
    }

    #[test]
    fn empty_artifact_list_is_an_error() {
        let f = fixture("machine: {}\n", None);
        // Overwrite the manifest with an empty x86_64 list.
        std::fs::write(&f.manifest, "[artifact]\nx86_64 = []\n").unwrap();
        let err = build(&f.fetcher, &opts(&f, false, false)).unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("pin") || msg.contains("empty"),
            "error should mention pin/empty: {msg}"
        );
        assert!(!f.out.exists());
    }

    /// The shipped CI example must always parse against the live config schema:
    /// this pins examples/node-ci.yaml against config-schema drift forever.
    #[test]
    fn ci_example_config_parses() {
        let yaml = include_str!("../../../examples/node-ci.yaml");
        machined_config::load_from_str(yaml)
            .expect("examples/node-ci.yaml must parse against the config schema");
    }

    #[test]
    fn checksum_mismatch_aborts() {
        let wrong = "00".repeat(32);
        let f = fixture("machine: {}\n", Some(&wrong));
        let err = build(&f.fetcher, &opts(&f, false, false)).unwrap_err();
        assert!(
            err.to_string().contains("sha256 mismatch"),
            "checksum mismatch must propagate: {err}"
        );
        assert!(!f.out.exists(), "no image on checksum mismatch");
    }
}
