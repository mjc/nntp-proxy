//! TLS configuration and handshake management for NNTP connections
//!
//! This module provides high-performance TLS support using rustls with optimizations:
//! - Ring crypto provider for fastest cryptographic operations
//! - TLS 1.3 early data (0-RTT) enabled for faster reconnections
//! - Session resumption enabled to avoid full handshakes
//! - Pure Rust implementation (memory safe, no C dependencies)
//! - System certificate loading with Mozilla CA bundle fallback

use crate::connection_error::ConnectionError;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme,
};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tracing::{debug, warn};

/// Configuration for TLS connections
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Enable TLS for this connection
    pub use_tls: bool,
    /// Verify server certificates (recommended: true)  
    pub tls_verify_cert: bool,
    /// Path to custom CA certificate file (optional)
    pub tls_cert_path: Option<String>,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            use_tls: false,
            tls_verify_cert: true,
            tls_cert_path: None,
        }
    }
}

/// Certificate loading results
#[derive(Debug)]
pub struct CertificateLoadResult {
    pub root_store: RootCertStore,
    pub sources: Vec<String>,
}

/// Custom certificate verifier that accepts all certificates (INSECURE!)
///
/// This is used when `tls_verify_cert = false` for NNTP servers without valid certificates.
/// **WARNING**: This disables all certificate validation and should only be used for testing
/// or with trusted private networks.
#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        // Accept all certificates without verification
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        // Accept all signatures without verification
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        // Accept all signatures without verification
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // Support all signature schemes
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

/// High-performance TLS connector with cached configuration
///
/// Caches the parsed TLS configuration including certificates to avoid
/// expensive re-parsing on every connection. Certificates are loaded once
/// during initialization and reused for all connections.
pub struct TlsManager {
    config: TlsConfig,
    /// Cached TLS connector with pre-loaded certificates
    ///
    /// Avoids expensive certificate parsing overhead (DER parsing, X.509 validation,
    /// signature verification) on every connection by loading certificates once at init.
    cached_connector: Arc<TlsConnector>,
}

impl std::fmt::Debug for TlsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsManager")
            .field("config", &self.config)
            .field("cached_connector", &"<TlsConnector>")
            .finish()
    }
}

impl Clone for TlsManager {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            cached_connector: Arc::clone(&self.cached_connector),
        }
    }
}

impl TlsManager {
    /// Create a new TLS manager with the given configuration
    ///
    /// **Performance**: Loads and parses certificates once during initialization
    /// instead of on every connection, eliminating certificate parsing overhead
    /// (DER parsing, X.509 validation, signature verification).
    pub fn new(config: TlsConfig) -> Result<Self, anyhow::Error> {
        // Load certificates once during initialization
        let cert_result = Self::load_certificates_sync(&config)?;
        let client_config = Self::create_optimized_config_inner(cert_result.root_store, &config)?;

        debug!(
            "TLS: Initialized with certificate sources: {}",
            cert_result.sources.join(", ")
        );

        let cached_connector = Arc::new(TlsConnector::from(Arc::new(client_config)));

        Ok(Self {
            config,
            cached_connector,
        })
    }

    /// Perform TLS handshake with optimizations for maximum performance
    ///
    /// **Performance**: Uses pre-loaded certificates from cache, no parsing overhead.
    pub async fn handshake(
        &self,
        stream: TcpStream,
        hostname: &str,
        backend_name: &str,
    ) -> Result<TlsStream<TcpStream>, anyhow::Error> {
        use anyhow::Context;
        
        let domain = rustls_pki_types::ServerName::try_from(hostname)
            .context("Invalid hostname for TLS")?
            .to_owned();

        debug!("TLS: Connecting to {} with cached config", hostname);

        self.cached_connector
            .connect(domain, stream)
            .await
            .map_err(|e| {
                ConnectionError::TlsHandshake {
                    backend: backend_name.to_string(),
                    source: Box::new(e),
                }
                .into()
            })
    }

    /// Load certificates from various sources with fallback chain (synchronous for init)
    fn load_certificates_sync(config: &TlsConfig) -> Result<CertificateLoadResult, anyhow::Error> {
        let mut root_store = RootCertStore::empty();
        let mut sources = Vec::new();

        // 1. Load custom CA certificate if provided
        if let Some(cert_path) = &config.tls_cert_path {
            debug!("TLS: Loading custom CA certificate from: {}", cert_path);
            Self::load_custom_certificate_sync(&mut root_store, cert_path)?;
            sources.push("custom certificate".to_string());
        }

        // 2. Try to load system certificates
        let system_count = Self::load_system_certificates_sync(&mut root_store)?;
        if system_count > 0 {
            debug!(
                "TLS: Loaded {} certificates from system store",
                system_count
            );
            sources.push("system certificates".to_string());
        }

        // 3. Fallback to Mozilla CA bundle if no certificates loaded
        if root_store.is_empty() {
            debug!("TLS: No system certificates available, using Mozilla CA bundle fallback");
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            sources.push("Mozilla CA bundle".to_string());
        }

        Ok(CertificateLoadResult {
            root_store,
            sources,
        })
    }

    /// Load custom certificate from file
    fn load_custom_certificate_sync(
        root_store: &mut RootCertStore,
        cert_path: &str,
    ) -> Result<(), anyhow::Error> {
        use anyhow::Context;
        
        let cert_data = std::fs::read(cert_path)
            .with_context(|| format!("Failed to read TLS certificate from {}", cert_path))?;

        let certs = rustls_pemfile::certs(&mut cert_data.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to parse TLS certificate")?;

        for cert in certs {
            root_store
                .add(cert)
                .context("Failed to add custom certificate to store")?;
        }

        Ok(())
    }

    /// Load system certificates, returning count of successfully loaded certificates
    fn load_system_certificates_sync(
        root_store: &mut RootCertStore,
    ) -> Result<usize, anyhow::Error> {
        let cert_result = rustls_native_certs::load_native_certs();
        let mut added_count = 0;

        for cert in cert_result.certs {
            if root_store.add(cert).is_ok() {
                added_count += 1;
            }
        }

        // Log any errors but don't fail - we have fallback
        for error in cert_result.errors {
            warn!("TLS: Certificate loading error: {}", error);
        }

        Ok(added_count)
    }

    /// Create optimized client configuration using ring crypto provider
    fn create_optimized_config_inner(
        root_store: RootCertStore,
        config: &TlsConfig,
    ) -> Result<ClientConfig, anyhow::Error> {
        use anyhow::Context;
        
        let mut client_config = if config.tls_verify_cert {
            debug!("TLS: Certificate verification enabled with ring crypto provider");
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .context("Failed to create TLS config with ring provider")?
                .with_root_certificates(root_store)
                .with_no_client_auth()
        } else {
            warn!(
                "TLS: Certificate verification DISABLED - this is insecure and should only be used for testing!"
            );
            // Use custom verifier that accepts all certificates
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .context("Failed to create TLS config with ring provider")?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerifier))
                .with_no_client_auth()
        };

        // Performance optimizations
        client_config.enable_early_data = true; // Enable TLS 1.3 0-RTT for faster reconnections
        client_config.resumption = rustls::client::Resumption::default(); // Enable session resumption

        Ok(client_config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tls_config_default() {
        let config = TlsConfig::default();
        assert!(!config.use_tls);
        assert!(config.tls_verify_cert);
        assert!(config.tls_cert_path.is_none());
    }

    #[test]
    fn test_tls_manager_creation() {
        let config = TlsConfig::default();
        let manager = TlsManager::new(config).unwrap();
        // Manager should successfully initialize with cached config
        assert!(Arc::strong_count(&manager.cached_connector) >= 1);
    }

    #[test]
    fn test_certificate_loading() {
        let config = TlsConfig::default();

        let result = TlsManager::load_certificates_sync(&config).unwrap();
        assert!(!result.root_store.is_empty());
        // Should have at least one source (system certificates or Mozilla CA bundle)
        assert!(!result.sources.is_empty());
        // Common sources include "system certificates" or "Mozilla CA bundle"
        assert!(
            result
                .sources
                .iter()
                .any(|s| s.contains("Mozilla") || s.contains("system"))
        );
    }
}
