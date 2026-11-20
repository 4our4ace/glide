//! TLS 1.3 identity generation and SHA-256 certificate fingerprint verification for QUIC.

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

pub const ALPN_GLIDE_V1: &[u8] = b"glide/1";

/// Ensures the global `ring` crypto provider is installed for `rustls`.
pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Persistent Ed25519 TLS identity for a Glide server.
pub struct ServerIdentity {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
    pub fingerprint: String,
}

impl ServerIdentity {
    /// Loads an existing identity from `dir` or generates and persists a new Ed25519 certificate.
    pub fn load_or_generate(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create identity directory {}", dir.display()))?;
        let cert_path = dir.join("server.cert.der");
        let key_path = dir.join("server.key.der");

        if cert_path.exists() && key_path.exists() {
            let cert_bytes = fs::read(&cert_path)
                .with_context(|| format!("failed to read {}", cert_path.display()))?;
            let key_bytes = fs::read(&key_path)
                .with_context(|| format!("failed to read {}", key_path.display()))?;
            let fingerprint = sha256_fingerprint(&cert_bytes);
            return Ok(Self {
                cert_der: CertificateDer::from(cert_bytes),
                key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes)),
                fingerprint,
            });
        }

        let mut params = rcgen::CertificateParams::new(vec!["glide.local".to_string()])
            .context("failed to build certificate params")?;
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("Glide Software KVM".to_string()),
        );
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
            .context("failed to generate Ed25519 key")?;
        let cert = params
            .self_signed(&key_pair)
            .context("failed to self-sign certificate")?;

        let cert_bytes = cert.der().to_vec();
        let key_bytes = key_pair.serialize_der();

        fs::write(&cert_path, &cert_bytes)
            .with_context(|| format!("failed to write {}", cert_path.display()))?;
        fs::write(&key_path, &key_bytes)
            .with_context(|| format!("failed to write {}", key_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600));
        }

        let fingerprint = sha256_fingerprint(&cert_bytes);
        info!(%fingerprint, path = %dir.display(), "Generated new Ed25519 TLS identity");

        Ok(Self {
            cert_der: CertificateDer::from(cert_bytes),
            key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes)),
            fingerprint,
        })
    }

    /// Generates an ephemeral in-memory identity (primarily for tests).
    pub fn generate_ephemeral() -> Result<Self> {
        let params = rcgen::CertificateParams::new(vec!["glide.local".to_string()])?;
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let cert = params.self_signed(&key_pair)?;
        let cert_bytes = cert.der().to_vec();
        let key_bytes = key_pair.serialize_der();
        let fingerprint = sha256_fingerprint(&cert_bytes);
        Ok(Self {
            cert_der: CertificateDer::from(cert_bytes),
            key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes)),
            fingerprint,
        })
    }
}

/// Computes a colon-separated lowercase SHA-256 fingerprint of raw DER certificate bytes.
pub fn sha256_fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Normalizes a user-supplied fingerprint (strips `SHA256:`, colons, whitespace, and lowercases).
pub fn normalize_fingerprint(fp: &str) -> String {
    let stripped = fp
        .trim()
        .strip_prefix("SHA256:")
        .or_else(|| fp.trim().strip_prefix("sha256:"))
        .unwrap_or(fp.trim());
    stripped
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Client trust policy for authenticating the Glide server's self-signed TLS certificate.
#[derive(Debug, Clone)]
pub enum TrustMode {
    /// Strict pinned SHA-256 fingerprint.
    Pinned(String),
    /// Trust On First Use: saves the server's fingerprint to `store_path` on first connect
    /// and enforces it on every subsequent connection.
    Tofu { store_path: PathBuf },
    /// Skip certificate fingerprint check (development/debugging only).
    InsecureSkipVerify,
}

#[derive(Debug)]
pub(crate) struct FingerprintVerifier {
    mode: TrustMode,
    provider: Arc<CryptoProvider>,
}

impl FingerprintVerifier {
    pub fn new(mode: TrustMode) -> Self {
        ensure_crypto_provider();
        Self {
            mode,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        let actual_pretty = sha256_fingerprint(end_entity.as_ref());
        let actual_norm = normalize_fingerprint(&actual_pretty);

        match &self.mode {
            TrustMode::Pinned(expected) => {
                let expected_norm = normalize_fingerprint(expected);
                if actual_norm == expected_norm && actual_norm.len() == 64 {
                    Ok(ServerCertVerified::assertion())
                } else {
                    Err(TlsError::General(format!(
                        "server fingerprint mismatch: got {actual_pretty}, expected {expected}"
                    )))
                }
            }
            TrustMode::Tofu { store_path } => {
                if store_path.exists() {
                    let stored = fs::read_to_string(store_path).map_err(|e| {
                        TlsError::General(format!(
                            "failed reading TOFU fingerprint at {}: {e}",
                            store_path.display()
                        ))
                    })?;
                    let stored_norm = normalize_fingerprint(&stored);
                    if actual_norm == stored_norm {
                        Ok(ServerCertVerified::assertion())
                    } else {
                        Err(TlsError::General(format!(
                            "TOFU FINGERPRINT CHANGED! Got {actual_pretty}, stored {}. \
                             Remove {} if you intentionally regenerated the server identity.",
                            stored.trim(),
                            store_path.display()
                        )))
                    }
                } else {
                    if let Some(parent) = store_path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    if let Err(e) = fs::write(store_path, format!("{actual_pretty}\n")) {
                        warn!(error = %e, "Failed to persist TOFU fingerprint");
                    } else {
                        info!(
                            fingerprint = %actual_pretty,
                            path = %store_path.display(),
                            "Trusted server fingerprint on first use (TOFU)"
                        );
                    }
                    Ok(ServerCertVerified::assertion())
                }
            }
            TrustMode::InsecureSkipVerify => {
                warn!(fingerprint = %actual_pretty, "Skipping TLS fingerprint check (--insecure)");
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
