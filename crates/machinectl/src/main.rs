//! machinectl — the machined management CLI (mutual-TLS gRPC client).

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};
use machined_apiserver::pb::machine_service_client::MachineServiceClient;
use machined_apiserver::pb::{Empty, ListResourcesRequest, UpgradeRequest};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// machined management CLI.
#[derive(Parser)]
#[command(name = "machinectl", version)]
struct Cli {
    /// Directory holding ca.pem, client.pem, client.key.
    #[arg(long, default_value = "/system/state/pki/machinectl")]
    bundle: PathBuf,
    /// API endpoint.
    #[arg(long, default_value = "https://127.0.0.1:50000")]
    endpoint: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the machined version.
    Version,
    /// List resources of a type (e.g. ServiceStatus, DiskStatus, TimeStatus).
    Get {
        /// Resource type name.
        resource_type: String,
        /// Resource namespace.
        #[arg(long, default_value = "runtime")]
        namespace: String,
    },
    /// Reboot the node.
    Reboot,
    /// Power the node off.
    Shutdown,
    /// Wipe STATE + EPHEMERAL and reboot to reprovision (DESTRUCTIVE).
    Reset {
        /// Confirm the destructive reset.
        #[arg(long)]
        yes: bool,
    },
    /// Upgrade the node to a new image bundle (downloads, verifies, kexecs).
    Upgrade {
        /// HTTP(S) URL of the upgrade bundle (.tar.gz of vmlinuz + initramfs.img).
        url: String,
        /// Expected sha256 (hex) of the bundle.
        sha256: String,
    },
}

fn read(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))
}

/// Build a mutual-TLS client from the bundle directory.
async fn connect(bundle: &Path, endpoint: &str) -> anyhow::Result<MachineServiceClient<Channel>> {
    let ca = read(&bundle.join("ca.pem"))?;
    let cert = read(&bundle.join("client.pem"))?;
    let key = read(&bundle.join("client.key"))?;
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca))
        .identity(Identity::from_pem(cert, key))
        .domain_name("127.0.0.1");
    // A management CLI must never block forever on a dead node. With QEMU
    // user-net (slirp), the host-side connect() to the forwarded port succeeds
    // instantly even when the guest is dead, then the TLS handshake blocks
    // indefinitely — so bound both the connect and the per-request time.
    let channel = Endpoint::from_shared(endpoint.to_string())?
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .tls_config(tls)?
        .connect()
        .await?;
    Ok(MachineServiceClient::new(channel))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Command::Reset { yes: false } = cli.command {
        eprintln!("reset wipes STATE and EPHEMERAL; pass --yes to confirm");
        std::process::exit(2);
    }
    let mut client = connect(&cli.bundle, &cli.endpoint).await?;
    match cli.command {
        Command::Version => {
            let v = client.version(Empty {}).await?.into_inner();
            println!("version={} image_id={}", v.version, v.image_id);
        }
        Command::Get {
            resource_type,
            namespace,
        } => {
            let resp = client
                .list_resources(ListResourcesRequest {
                    namespace,
                    r#type: resource_type,
                })
                .await?
                .into_inner();
            for e in resp.entries {
                let fields = e
                    .fields
                    .iter()
                    .map(|kv| format!("{}={}", kv.key, kv.value))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("{}\t{}", e.id, fields);
            }
        }
        Command::Reboot => {
            client.reboot(Empty {}).await?;
            println!("reboot requested");
        }
        Command::Shutdown => {
            client.shutdown(Empty {}).await?;
            println!("shutdown requested");
        }
        Command::Reset { .. } => {
            client.reset(Empty {}).await?;
            println!("reset requested");
        }
        Command::Upgrade { url, sha256 } => {
            client.upgrade(UpgradeRequest { url, sha256 }).await?;
            println!("upgrade requested");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_get_with_namespace() {
        let cli = Cli::try_parse_from([
            "machinectl",
            "--bundle",
            "/tmp/b",
            "get",
            "ServiceStatus",
            "--namespace",
            "block",
        ])
        .unwrap();
        match cli.command {
            Command::Get {
                resource_type,
                namespace,
            } => {
                assert_eq!(resource_type, "ServiceStatus");
                assert_eq!(namespace, "block");
            }
            _ => panic!("expected Get"),
        }
        assert_eq!(cli.bundle, PathBuf::from("/tmp/b"));
    }

    #[test]
    fn version_defaults() {
        let cli = Cli::try_parse_from(["machinectl", "version"]).unwrap();
        assert!(matches!(cli.command, Command::Version));
        assert_eq!(cli.endpoint, "https://127.0.0.1:50000");
    }

    #[test]
    fn parses_reboot_and_shutdown() {
        let r = Cli::try_parse_from(["machinectl", "reboot"]).unwrap();
        assert!(matches!(r.command, Command::Reboot));
        let s = Cli::try_parse_from(["machinectl", "shutdown"]).unwrap();
        assert!(matches!(s.command, Command::Shutdown));
    }

    #[test]
    fn parses_reset_with_and_without_yes() {
        let r = Cli::try_parse_from(["machinectl", "reset", "--yes"]).unwrap();
        assert!(matches!(r.command, Command::Reset { yes: true }));
        let r2 = Cli::try_parse_from(["machinectl", "reset"]).unwrap();
        assert!(matches!(r2.command, Command::Reset { yes: false }));
    }
}
