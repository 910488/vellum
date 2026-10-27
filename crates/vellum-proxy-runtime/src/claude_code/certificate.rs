use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Datelike;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};

/// Serialize only into the host's encrypted credential store. No Debug impl.
/// The CA signing key is discarded immediately after signing the one leaf.
#[derive(Serialize, Deserialize)]
pub struct CodeCertificate {
    #[serde(default)]
    format_version: u8,
    pub ca_pem: String,
    ca_der: Vec<u8>,
    leaf_der: Vec<u8>,
    leaf_key: Vec<u8>,
}

impl CodeCertificate {
    pub fn generate() -> Result<Self, String> {
        let mut ca = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
        let start = chrono::Utc::now() - chrono::Duration::days(1);
        let end = start + chrono::Duration::days(730);
        ca.not_before = rcgen::date_time_ymd(start.year(), start.month() as u8, start.day() as u8);
        ca.not_after = rcgen::date_time_ymd(end.year(), end.month() as u8, end.day() as u8);
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "Vellum Claude Code local proxy");
        ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        ca.use_authority_key_identifier_extension = true;
        ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca.name_constraints = Some(rcgen::NameConstraints {
            permitted_subtrees: vec![rcgen::GeneralSubtree::DnsName("api.anthropic.com".into())],
            excluded_subtrees: vec![],
        });
        let ca_key = KeyPair::generate().map_err(err)?;
        let ca_cert = ca.self_signed(&ca_key).map_err(err)?;
        let mut leaf = CertificateParams::new(vec!["api.anthropic.com".into()]).map_err(err)?;
        leaf.not_before = ca_cert.params().not_before;
        leaf.not_after = ca_cert.params().not_after;
        leaf.is_ca = IsCa::ExplicitNoCa;
        leaf.use_authority_key_identifier_extension = true;
        leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let key = KeyPair::generate().map_err(err)?;
        let cert = leaf.signed_by(&key, &ca_cert, &ca_key).map_err(err)?;
        let encoded = STANDARD.encode(ca_cert.der());
        let lines = encoded
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Self {
            format_version: 1,
            ca_pem: format!("-----BEGIN CERTIFICATE-----\n{lines}\n-----END CERTIFICATE-----\n"),
            ca_der: ca_cert.der().to_vec(),
            leaf_der: cert.der().to_vec(),
            leaf_key: key.serialize_der(),
        })
    }

    pub fn is_current(&self) -> bool {
        self.format_version == 1
    }

    pub fn ca_der(&self) -> &[u8] {
        &self.ca_der
    }

    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, String> {
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(err)?
        .with_no_client_auth()
        .with_single_cert(
            vec![
                CertificateDer::from(self.leaf_der.clone()),
                CertificateDer::from(self.ca_der.clone()),
            ],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.leaf_key.clone())),
        )
        .map_err(err)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
