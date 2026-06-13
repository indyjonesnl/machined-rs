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
///
/// The returned order is suitable for blind `finit_module()` at boot: every
/// module appears after all of its (transitive) dependencies, with no
/// duplicates. Module names treat `-` and `_` as equivalent per kernel
/// convention.
///
/// # Errors
///
/// Returns an error if any name in `roots` is not declared as a line in
/// `modules_dep`.
pub fn resolve_closure(modules_dep: &str, roots: &[&str]) -> anyhow::Result<Vec<String>> {
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new(); // path -> dep paths
    let mut by_name: BTreeMap<String, String> = BTreeMap::new(); // name -> path
    for line in modules_dep.lines() {
        let Some((module, rest)) = line.split_once(':') else {
            continue;
        };
        let path = strip_gz(module.trim());
        by_name.insert(module_name(&path), path.clone());
        deps.insert(path, rest.split_whitespace().map(strip_gz).collect());
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // The seen-set is inserted before recursing into a path's deps, so a cyclic
    // modules.dep (malformed; the kernel guarantees acyclic) terminates instead
    // of recursing forever.
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

/// The Raspberry Pi (linux-rpi) initramfs module roots. EMPTY: the Pi kernel
/// builds the SD/MMC host (sdhci-iproc, bcm2835-sdhost), ext4, fat/vfat, and
/// nls_cp437 in — the initramfs needs no storage/fs modules for an SD boot.
pub const PI_MODULES: &[&str] = &[];

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
        let pos = |n: &str| {
            order
                .iter()
                .position(|p| p.ends_with(&format!("/{n}.ko")))
                .unwrap()
        };
        assert!(pos("virtio") < pos("virtio_blk"));
        assert!(pos("jbd2") < pos("ext4"));
        assert!(pos("crc16") < pos("ext4"));
        assert_eq!(order.len(), 5, "no duplicates, nothing extra: {order:?}");
        assert!(
            order.iter().all(|p| p.ends_with(".ko")),
            "gz suffix stripped"
        );
    }

    #[test]
    fn shared_dep_across_roots_appears_exactly_once() {
        // Diamond: ext4 and xfs both pull crc32c; the second root's traversal
        // must hit the seen-set and not re-emit (or reorder) the shared dep.
        const DIAMOND: &str = "\
kernel/fs/ext4/ext4.ko.gz: kernel/lib/crc32c.ko.gz
kernel/fs/xfs/xfs.ko.gz: kernel/lib/crc32c.ko.gz
kernel/lib/crc32c.ko.gz:
";
        let order = resolve_closure(DIAMOND, &["ext4", "xfs"]).unwrap();
        let occurrences = order.iter().filter(|p| p.ends_with("/crc32c.ko")).count();
        assert_eq!(occurrences, 1, "shared dep emitted exactly once: {order:?}");
        let pos = |n: &str| {
            order
                .iter()
                .position(|p| p.ends_with(&format!("/{n}.ko")))
                .unwrap()
        };
        assert!(pos("crc32c") < pos("ext4"));
        assert!(pos("crc32c") < pos("xfs"));
        assert_eq!(order.len(), 3, "{order:?}");
    }

    #[test]
    fn empty_roots_yields_no_modules() {
        // The Pi build passes PI_MODULES (empty): no roots → no module paths.
        let order = resolve_closure(DEP, &[]).unwrap();
        assert!(
            order.is_empty(),
            "empty roots resolve to no modules: {order:?}"
        );
    }

    #[test]
    fn unknown_module_is_an_error() {
        let err = resolve_closure(DEP, &["nvme"]).unwrap_err();
        assert!(err.to_string().contains("nvme"), "{err}");
    }
}
