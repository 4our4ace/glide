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
        write_private_file(&key_path, &key_bytes)?;

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

/// Atomically creates/truncates a private key file with `0o600` permissions on Unix
/// before writing secret key bytes.
#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create private key file {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to enforce 0600 permissions on {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    fs::write(path, contents)
        .with_context(|| format!("failed to write {}", path.display()))
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

/// Generates 32 cryptographically secure random bytes encoded as 64 lowercase hex characters.
pub fn random_hex_token_32() -> Result<String> {
    let mut bytes = [0u8; 32];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("CSPRNG failed generating auth token"))?;
    let mut out = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

/// Strictly validates and normalizes a SHA-256 certificate fingerprint.
///
/// Accepts an optional `SHA256:` / `sha256:` prefix and optional `:` / whitespace separators,
/// rejecting any non-hexadecimal characters, malformed colon placement, or lengths other than
/// exactly 64 hex characters (32 SHA-256 bytes).
pub fn normalize_fingerprint(fp: &str) -> Option<String> {
    let trimmed = fp.trim();
    let stripped = trimmed
        .strip_prefix("SHA256:")
        .or_else(|| trimmed.strip_prefix("sha256:"))
        .unwrap_or(trimmed)
        .trim();

    if stripped.is_empty() || stripped.starts_with(':') || stripped.ends_with(':') {
        return None;
    }

    let mut hex = String::with_capacity(64);
    let mut prev_colon = false;
    for c in stripped.chars() {
        if c.is_ascii_hexdigit() {
            hex.push(c.to_ascii_lowercase());
            prev_colon = false;
        } else if c == ':' {
            if prev_colon {
                return None;
            }
            prev_colon = true;
        } else if c.is_ascii_whitespace() {
            continue;
        } else {
            return None;
        }
    }

    if hex.len() == 64 {
        Some(hex)
    } else {
        None
    }
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
        let Some(actual_norm) = normalize_fingerprint(&actual_pretty) else {
            return Err(TlsError::General(
                "internal error computing server certificate fingerprint".into(),
            ));
        };

        match &self.mode {
            TrustMode::Pinned(expected) => {
                let Some(expected_norm) = normalize_fingerprint(expected) else {
                    return Err(TlsError::General(format!(
                        "invalid pinned server fingerprint '{expected}': expected 64 hex characters (SHA-256)"
                    )));
                };
                if actual_norm == expected_norm {
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
                    let Some(stored_norm) = normalize_fingerprint(&stored) else {
                        return Err(TlsError::General(format!(
                            "corrupted TOFU fingerprint stored at {}",
                            store_path.display()
                        )));
                    };
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
                        if !parent.as_os_str().is_empty() {
                            fs::create_dir_all(parent).map_err(|e| {
                                TlsError::General(format!(
                                    "failed to create TOFU store directory {}: {e}",
                                    parent.display()
                                ))
                            })?;
                        }
                    }
                    fs::write(store_path, format!("{actual_pretty}\n")).map_err(|e| {
                        TlsError::General(format!(
                            "failed to persist TOFU fingerprint at {}: {e}",
                            store_path.display()
                        ))
                    })?;
                    info!(
                        fingerprint = %actual_pretty,
                        path = %store_path.display(),
                        "Trusted server fingerprint on first use (TOFU)"
                    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "glide-tls-test-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn normalize_fingerprint_strictly_validates_sha256_hex() {
        let valid_pretty = sha256_fingerprint(b"sample-cert-der");
        let valid_hex = valid_pretty.replace(':', "");
        assert_eq!(valid_hex.len(), 64);

        assert_eq!(normalize_fingerprint(&valid_pretty), Some(valid_hex.clone()));
        assert_eq!(
            normalize_fingerprint(&format!("SHA256:{valid_pretty}\n")),
            Some(valid_hex.clone())
        );
        assert_eq!(
            normalize_fingerprint(&format!("sha256:{}", valid_hex.to_ascii_uppercase())),
            Some(valid_hex.clone())
        );

        // Reject short, oversized, empty, non-hex, and malformed colon forms
        assert_eq!(normalize_fingerprint(""), None);
        assert_eq!(normalize_fingerprint("aa:bb:cc:dd"), None);
        assert_eq!(normalize_fingerprint(&format!("{valid_hex}00")), None);
        assert_eq!(normalize_fingerprint(&format!("zz{valid_hex}")), None);
        assert_eq!(normalize_fingerprint(&format!(":{valid_pretty}")), None);
        assert_eq!(normalize_fingerprint(&format!("{valid_pretty}:")), None);
        assert_eq!(
            normalize_fingerprint(&valid_pretty.replacen(':', "::", 1)),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_or_generate_creates_private_key_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = unique_temp_dir("key-perms");
        let id = ServerIdentity::load_or_generate(&dir).unwrap();
        assert!(normalize_fingerprint(&id.fingerprint).is_some());

        let key_meta = fs::metadata(dir.join("server.key.der")).unwrap();
        assert_eq!(key_meta.permissions().mode() & 0o777, 0o600);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tofu_fails_closed_when_store_cannot_be_persisted() {
        let dir = unique_temp_dir("tofu-fail-closed");
        // Put a regular file where a parent directory would need to be created.
        let blocking_file = dir.join("not-a-dir");
        fs::write(&blocking_file, b"x").unwrap();
        let bad_store_path = blocking_file.join("tofu.txt");

        let id = ServerIdentity::generate_ephemeral().unwrap();
        let verifier = FingerprintVerifier::new(TrustMode::Tofu {
            store_path: bad_store_path,
        });
        let server_name = ServerName::try_from("glide.local").unwrap();
        let res = verifier.verify_server_cert(
            &id.cert_der,
            &[],
            &server_name,
            &[],
            UnixTime::now(),
        );
        assert!(res.is_err(), "TOFU must fail closed when persistence fails");

        // Valid store path persists on first use and rejects a different cert on second use.
        let good_store_path = dir.join("nested").join("tofu.txt");
        let verifier_ok = FingerprintVerifier::new(TrustMode::Tofu {
            store_path: good_store_path.clone(),
        });
        verifier_ok
            .verify_server_cert(&id.cert_der, &[], &server_name, &[], UnixTime::now())
            .unwrap();
        assert!(good_store_path.exists());

        let other_id = ServerIdentity::generate_ephemeral().unwrap();
        assert!(verifier_ok
            .verify_server_cert(&other_id.cert_der, &[], &server_name, &[], UnixTime::now())
            .is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
