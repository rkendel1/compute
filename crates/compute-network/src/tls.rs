//! Certificates: parsing, validity, and fingerprints.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TlsError(pub String);

/// What is public about a certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateInfo {
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    /// `sha256:` of the leaf certificate's DER.
    pub fingerprint: String,
    /// Subject alternative DNS names.
    pub names: Vec<String>,
}

/// The certificates in a PEM chain, leaf first.
pub fn certificates(chain_pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let chain = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsError(format!("certificate chain: {error}")))?;
    if chain.is_empty() {
        return Err(TlsError("the certificate chain is empty".into()));
    }
    Ok(chain)
}

pub fn info(chain_pem: &[u8]) -> Result<CertificateInfo, TlsError> {
    let chain = certificates(chain_pem)?;
    let leaf = &chain[0];
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|error| TlsError(format!("certificate: {error}")))?;
    let validity = parsed.validity();
    let time = |value: &x509_parser::time::ASN1Time| {
        DateTime::<Utc>::from_timestamp(value.timestamp(), 0)
            .ok_or_else(|| TlsError("certificate validity is out of range".into()))
    };
    let mut names = vec![];
    if let Ok(Some(extension)) = parsed.subject_alternative_name() {
        for name in &extension.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(name) = name {
                names.push((*name).to_string());
            }
        }
    }
    Ok(CertificateInfo {
        not_before: time(&validity.not_before)?,
        not_after: time(&validity.not_after)?,
        fingerprint: fingerprint(leaf.as_ref()),
        names,
    })
}

pub fn fingerprint(der: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(der))
}

/// A chain and its key, ready to serve.
pub fn certified_key(chain_pem: &[u8], key_pem: &[u8]) -> Result<Arc<CertifiedKey>, TlsError> {
    let chain = certificates(chain_pem)?;
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|error| TlsError(format!("private key: {error}")))?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|error| TlsError(format!("private key: {error}")))?;
    Ok(Arc::new(CertifiedKey::new(chain, signing)))
}
