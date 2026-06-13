//! gen-pki: produce a complete pre-baked node PKI dir on the build host.
//!
//! SECURITY: a pre-baked PKI dir ends up world-readable on the image's FAT boot
//! partition (FAT carries no unix permissions). Operators MUST treat the
//! resulting image / SD card as containing private key material.

use std::path::Path;

pub fn gen_pki(out: &Path) -> anyhow::Result<()> {
    let pki = machined_pki::NodePki::load_or_generate(
        out,
        "node",
        &["127.0.0.1".into(), "localhost".into()],
    )?;
    machined_pki::write_client_bundle(&out.join("machinectl"), &pki, "machinectl")?;
    println!("PKI written to {}", out.display());
    Ok(())
}
