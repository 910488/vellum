//! The certificate the Desktop backend relay serves `https://localhost` with.
//!
//! Codex Desktop takes its backend origin from the app-server's
//! `workspaceRouting.backendOrigin`, and accepts only an `https:` origin there.
//! So the relay has to speak TLS, under a certificate Desktop's Chromium
//! trusts. Vellum makes one per data root and asks the user to trust it.
//!
//! What the certificate can vouch for is kept as narrow as a self-signed one
//! allows, because the user is asked to put it among their trusted roots:
//! it names only `localhost` and the two loopback addresses, carries
//! `CA:FALSE` so nothing chains to it, and is good for server
//! authentication only. Its key leaving the machine would let someone
//! impersonate this user's own loopback, nothing else.
//!
//! It lasts a century so the trust prompt is a one-time event; the lifetime
//! caps browsers enforce apply to publicly trusted roots, not to a root the
//! user installed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Datelike, Duration, Utc};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CERT_FILE: &str = "localhost.cer";
const KEY_FILE: &str = "localhost.key";
const META_FILE: &str = "localhost.json";
const LIFETIME_YEARS: i32 = 100;
const COMMON_NAME: &str = "Vellum Desktop backend relay (localhost)";

#[derive(Debug, thiserror::Error)]
pub enum LocalhostCertificateError {
    #[error("certificate storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("certificate generation: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("certificate metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("TLS configuration: {0}")]
    Tls(#[from] rustls::Error),
}

/// What the trust tooling needs to find the certificate again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CertificateIdentity {
    /// Upper-case hex, as `certutil` prints and accepts it.
    pub serial_hex: String,
    /// SHA-256 of the DER encoding, lower-case hex.
    pub sha256: String,
}

pub struct LocalhostCertificate {
    cert_path: PathBuf,
    der: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    identity: CertificateIdentity,
}

impl LocalhostCertificate {
    /// The certificate under `dir`, made there first if there is none.
    ///
    /// A half-written or unreadable set is replaced rather than repaired: the
    /// replacement needs trusting again, which is better than serving a pair
    /// that does not match.
    pub fn load_or_create(dir: &Path) -> Result<Self, LocalhostCertificateError> {
        if let Some(existing) = Self::load(dir) {
            return Ok(existing);
        }
        Self::create(dir)
    }

    fn load(dir: &Path) -> Option<Self> {
        let der = std::fs::read(dir.join(CERT_FILE)).ok()?;
        let key = std::fs::read(dir.join(KEY_FILE)).ok()?;
        let identity: CertificateIdentity =
            serde_json::from_slice(&std::fs::read(dir.join(META_FILE)).ok()?).ok()?;
        if identity.sha256 != sha256_hex(&der) {
            return None;
        }
        let certificate = Self {
            cert_path: dir.join(CERT_FILE),
            der: CertificateDer::from(der),
            key: PrivatePkcs8KeyDer::from(key),
            identity,
        };
        // A key that does not belong to the certificate fails here, not on
        // Desktop's first handshake.
        certificate.server_config().ok()?;
        Some(certificate)
    }

    fn create(dir: &Path) -> Result<Self, LocalhostCertificateError> {
        let mut serial = [0u8; 16];
        getrandom::fill(&mut serial).map_err(|error| std::io::Error::other(error.to_string()))?;
        // Positive, so the DER INTEGER is the same 16 bytes certutil shows.
        serial[0] &= 0x7f;
        serial[0] |= 0x01;

        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])?;
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        let mut name = rcgen::DistinguishedName::new();
        name.push(rcgen::DnType::CommonName, COMMON_NAME);
        params.distinguished_name = name;
        params.is_ca = rcgen::IsCa::ExplicitNoCa;
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
        // A day of slack for a clock that runs behind; the end date is the
        // first of the month so it always exists.
        let start = Utc::now() - Duration::days(1);
        params.not_before =
            rcgen::date_time_ymd(start.year(), start.month() as u8, start.day() as u8);
        params.not_after =
            rcgen::date_time_ymd(start.year() + LIFETIME_YEARS, start.month() as u8, 1);

        let key_pair = rcgen::KeyPair::generate()?;
        let certificate = params.self_signed(&key_pair)?;
        let der = certificate.der().to_vec();
        let identity = CertificateIdentity {
            serial_hex: hex::encode_upper(serial),
            sha256: sha256_hex(&der),
        };

        std::fs::create_dir_all(dir)?;
        // The metadata goes last: a set without it is incomplete and is made
        // again on the next start.
        let _ = std::fs::remove_file(dir.join(META_FILE));
        std::fs::write(dir.join(KEY_FILE), key_pair.serialize_der())?;
        std::fs::write(dir.join(CERT_FILE), &der)?;
        std::fs::write(dir.join(META_FILE), serde_json::to_vec_pretty(&identity)?)?;
        Ok(Self {
            cert_path: dir.join(CERT_FILE),
            der: CertificateDer::from(der),
            key: PrivatePkcs8KeyDer::from(key_pair.serialize_der()),
            identity,
        })
    }

    /// The DER certificate file, which is what the OS trust tools import.
    pub fn cert_path(&self) -> &Path {
        &self.cert_path
    }

    pub fn identity(&self) -> &CertificateIdentity {
        &self.identity
    }

    pub fn der(&self) -> &CertificateDer<'static> {
        &self.der
    }

    /// HTTP/1.1 only: Desktop's dictation stream is a WebSocket, and an h2
    /// upgrade would need extended CONNECT on both sides.
    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, LocalhostCertificateError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![self.der.clone()],
                PrivateKeyDer::Pkcs8(self.key.clone_key()),
            )?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_certificate_is_made_once_and_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = LocalhostCertificate::load_or_create(dir.path()).unwrap();
        let again = LocalhostCertificate::load_or_create(dir.path()).unwrap();
        assert_eq!(first.identity(), again.identity());
        assert_eq!(first.der(), again.der());
        assert_eq!(first.identity().serial_hex.len(), 32);
    }

    #[test]
    fn a_tampered_set_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let first = LocalhostCertificate::load_or_create(dir.path()).unwrap();
        std::fs::write(dir.path().join(CERT_FILE), b"not a certificate").unwrap();
        let replaced = LocalhostCertificate::load_or_create(dir.path()).unwrap();
        assert_ne!(first.identity(), replaced.identity());
    }

    /// The handshake a client trusting exactly this certificate would make.
    #[tokio::test]
    async fn a_client_trusting_it_completes_a_localhost_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let certificate = LocalhostCertificate::load_or_create(dir.path()).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(certificate.server_config().unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = acceptor.accept(stream).await;
        });

        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate.der().clone()).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        connector.connect(name, stream).await.unwrap();
    }
}
