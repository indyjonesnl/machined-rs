//! Import pre-baked OCI archives from /boot/images into containerd's k8s.io
//! namespace, so the CRI sandbox + pod images are present offline. This is the
//! ONE containerd-specific runtime step in the daemon: it shells `ctr` (from
//! /boot/bin). Swapping the CRI runtime means swapping this importer.

use std::path::Path;
use std::time::Duration;

use machined_config::ctr_import_args;
use tracing::{info, warn};

const IMAGES_DIR: &str = "/boot/images";

/// Wait (bounded) for the CRI socket, then `ctr images import` every *.tar under
/// /boot/images. Best-effort: failures are logged; the PodController stays
/// Pending until the images appear, so a missed import just delays pod start.
pub async fn import_boot_images(socket: String) {
    let dir = Path::new(IMAGES_DIR);
    let tars: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "tar").unwrap_or(false))
            .collect(),
        Err(_) => return, // no /boot/images (off-image / no pods) — nothing to do.
    };
    if tars.is_empty() {
        return;
    }

    // Wait for containerd to create its socket (bounded ~60s).
    let sock = Path::new(&socket);
    for _ in 0..300 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    for tar in tars {
        let tar_s = tar.to_string_lossy().to_string();
        let args = ctr_import_args(&socket, &tar_s);
        match tokio::process::Command::new("ctr").args(&args).output().await {
            Ok(o) if o.status.success() => info!("imported image {tar_s}"),
            Ok(o) => warn!("ctr import {tar_s} failed: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => warn!("spawning ctr for {tar_s}: {e}"),
        }
    }
}
