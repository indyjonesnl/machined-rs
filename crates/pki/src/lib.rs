//! Node PKI: a self-signed CA and CA-signed server/client certificates (rcgen).
//! Everything is PEM in/out so `rustls`/`tonic` can consume it directly.

use std::fs;
use std::path::Path;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};

#[derive(thiserror::Error, Debug)]
pub enum PkiError {
    #[error("rcgen: {0}")]
    Rcgen(String),
    #[error("io {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("partial PKI dir; missing: {0:?}")]
    Partial(Vec<String>),
}

pub type Result<T> = std::result::Result<T, PkiError>;

fn rc<E: std::fmt::Display>(e: E) -> PkiError {
    PkiError::Rcgen(e.to_string())
}

/// A certificate + its private key, both PEM-encoded.
#[derive(Clone, Debug)]
pub struct CertKey {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Leaf certificate role (sets the Extended Key Usage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertRole {
    Server,
    Client,
}

/// Generate a self-signed CA.
pub fn generate_ca() -> Result<CertKey> {
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(rc)?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "machined-ca");
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let key = KeyPair::generate().map_err(rc)?;
    let cert = params.self_signed(&key).map_err(rc)?;
    Ok(CertKey {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// Generate a leaf certificate signed by `ca`, with the given common name, role
/// (serverAuth/clientAuth EKU), and Subject Alternative Names.
pub fn generate_cert(ca: &CertKey, cn: &str, role: CertRole, sans: &[String]) -> Result<CertKey> {
    let ca_key = KeyPair::from_pem(&ca.key_pem).map_err(rc)?;
    let ca_params = CertificateParams::from_ca_cert_pem(&ca.cert_pem).map_err(rc)?;
    let ca_cert = ca_params.self_signed(&ca_key).map_err(rc)?;

    let mut params = CertificateParams::new(sans.to_vec()).map_err(rc)?;
    params.distinguished_name.push(DnType::CommonName, cn);
    params.extended_key_usages = vec![match role {
        CertRole::Server => ExtendedKeyUsagePurpose::ServerAuth,
        CertRole::Client => ExtendedKeyUsagePurpose::ClientAuth,
    }];
    let key = KeyPair::generate().map_err(rc)?;
    let cert = params.signed_by(&key, &ca_cert, &ca_key).map_err(rc)?;
    Ok(CertKey {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// The node's persistent PKI: a CA and a server identity, load-or-generated.
#[derive(Debug)]
pub struct NodePki {
    ca: CertKey,
    server: CertKey,
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|source| PkiError::Io {
        path: path.to_string_lossy().to_string(),
        source,
    })
}

fn write(path: &Path, data: &str) -> Result<()> {
    fs::write(path, data).map_err(|source| PkiError::Io {
        path: path.to_string_lossy().to_string(),
        source,
    })
}

/// Write a private key, restricting it to owner read/write (0600) on Unix so the
/// CA/server private keys are never world-readable.
fn write_key(path: &Path, data: &str) -> Result<()> {
    write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            PkiError::Io {
                path: path.to_string_lossy().to_string(),
                source,
            }
        })?;
    }
    Ok(())
}

impl NodePki {
    /// Load the CA + server identity from `dir`, generating + persisting them if
    /// absent. Idempotent: a second call with the same dir loads the same CA.
    /// A PARTIAL dir (some of the four files missing) is an error — silently
    /// re-keying over remnants would orphan previously-issued client certs.
    pub fn load_or_generate(dir: &Path, server_cn: &str, server_sans: &[String]) -> Result<Self> {
        fs::create_dir_all(dir).map_err(|source| PkiError::Io {
            path: dir.to_string_lossy().to_string(),
            source,
        })?;
        // The PKI dir holds private keys: owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
                PkiError::Io {
                    path: dir.to_string_lossy().to_string(),
                    source,
                }
            })?;
        }
        let cap = dir.join("ca.pem");
        let cak = dir.join("ca.key");
        let sp = dir.join("server.pem");
        let sk = dir.join("server.key");

        let missing: Vec<&str> = [
            ("ca.pem", &cap),
            ("ca.key", &cak),
            ("server.pem", &sp),
            ("server.key", &sk),
        ]
        .iter()
        .filter(|(_, p)| !p.exists())
        .map(|(name, _)| *name)
        .collect();

        if missing.is_empty() {
            return Ok(Self {
                ca: CertKey {
                    cert_pem: read(&cap)?,
                    key_pem: read(&cak)?,
                },
                server: CertKey {
                    cert_pem: read(&sp)?,
                    key_pem: read(&sk)?,
                },
            });
        }
        if missing.len() != 4 {
            return Err(PkiError::Partial(
                missing.into_iter().map(String::from).collect(),
            ));
        }

        let ca = generate_ca()?;
        let server = generate_cert(&ca, server_cn, CertRole::Server, server_sans)?;
        write(&cap, &ca.cert_pem)?;
        write_key(&cak, &ca.key_pem)?;
        write(&sp, &server.cert_pem)?;
        write_key(&sk, &server.key_pem)?;
        Ok(Self { ca, server })
    }

    pub fn server_identity(&self) -> (String, String) {
        (self.server.cert_pem.clone(), self.server.key_pem.clone())
    }

    pub fn ca_pem(&self) -> String {
        self.ca.cert_pem.clone()
    }

    /// Issue a fresh client certificate signed by the node CA.
    pub fn issue_client(&self, cn: &str) -> Result<CertKey> {
        generate_cert(&self.ca, cn, CertRole::Client, &[])
    }
}

/// Write a client bundle (CA cert + a fresh client cert/key for `cn`) into
/// `dir`, creating it if needed. The client private key is owner-only (0600);
/// `ca.pem`/`client.pem` are world-readable certs.
pub fn write_client_bundle(dir: &Path, pki: &NodePki, cn: &str) -> Result<()> {
    fs::create_dir_all(dir).map_err(|source| PkiError::Io {
        path: dir.to_string_lossy().to_string(),
        source,
    })?;
    let client = pki.issue_client(cn)?;
    write(&dir.join("ca.pem"), &pki.ca_pem())?;
    write(&dir.join("client.pem"), &client.cert_pem)?;
    write_key(&dir.join("client.key"), &client.key_pem)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_pem(s: &str, label: &str) -> bool {
        s.contains(&format!("-----BEGIN {label}-----"))
            && s.contains(&format!("-----END {label}-----"))
    }

    #[test]
    fn generates_ca_and_leaf_pem() {
        let ca = generate_ca().unwrap();
        assert!(is_pem(&ca.cert_pem, "CERTIFICATE"));
        assert!(is_pem(&ca.key_pem, "PRIVATE KEY"));

        let server = generate_cert(&ca, "node", CertRole::Server, &["127.0.0.1".into()]).unwrap();
        assert!(is_pem(&server.cert_pem, "CERTIFICATE"));
        let client = generate_cert(&ca, "admin", CertRole::Client, &[]).unwrap();
        assert!(is_pem(&client.cert_pem, "CERTIFICATE"));
    }

    #[test]
    fn node_pki_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("mnd-pki-{}", std::process::id()));
        let p1 = NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        let p2 = NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        // Second call loads the same CA, not a fresh one.
        assert_eq!(p1.ca_pem(), p2.ca_pem());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_pki_dir_errors_not_rekeys() {
        let dir = std::env::temp_dir().join(format!("mnd-pki-part-{}", std::process::id()));
        NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        std::fs::remove_file(dir.join("server.key")).unwrap();
        let err = NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap_err();
        assert!(err.to_string().contains("server.key"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn pki_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mnd-pki-700-{}", std::process::id()));
        NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_client_bundle_emits_ca_cert_key() {
        let dir = std::env::temp_dir().join(format!("mnd-pki-bundle-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let pki = NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        let bundle = dir.join("machinectl");
        write_client_bundle(&bundle, &pki, "machinectl").unwrap();
        for f in ["ca.pem", "client.pem", "client.key"] {
            assert!(bundle.join(f).exists(), "{f}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(bundle.join("client.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "client.key must be 0600");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn private_key_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mnd-pki-perm-{}", std::process::id()));
        NodePki::load_or_generate(&dir, "node", &["127.0.0.1".into()]).unwrap();
        for key in ["ca.key", "server.key"] {
            let mode = std::fs::metadata(dir.join(key))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{key} must be 0600");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
