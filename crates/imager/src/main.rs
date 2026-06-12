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
