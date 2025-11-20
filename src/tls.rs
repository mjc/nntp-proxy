//! TLS configuration and handshake management for NNTP connections
//!
//! This module provides high-performance TLS support using rustls with optimizations:
//! - Ring crypto provider for pure Rust crypto operations
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
use tokio_rustls::TlsConnector;
use tracing::{debug, warn};

// Re-export TlsStream for use in other modules
pub use tokio_rustls::client::TlsStream;

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

impl TlsConfig {
    /// Create a builder for TlsConfig
    ///
    /// # Example
    /// ```
    /// use nntp_proxy::tls::TlsConfig;
    ///
    /// let config = TlsConfig::builder()
    ///     .enabled(true)
    ///     .verify_cert(true)
    ///     .build();
    /// ```
    pub fn builder() -> TlsConfigBuilder {
        TlsConfigBuilder::default()
    }
}

/// Builder for type-safe TLS configuration
///
/// Provides a fluent API for constructing TLS configurations with sensible defaults.
///
/// # Examples
///
/// Basic TLS with verification:
/// ```
/// use nntp_proxy::tls::TlsConfig;
///
/// let config = TlsConfig::builder()
///     .enabled(true)
///     .verify_cert(true)
///     .build();
/// ```
///
/// TLS with custom CA certificate:
/// ```
/// use nntp_proxy::tls::TlsConfig;
///
/// let config = TlsConfig::builder()
///     .enabled(true)
///     .verify_cert(true)
///     .cert_path("/path/to/ca.pem")
///     .build();
/// ```
///
/// Insecure TLS (for testing only):
/// ```
/// use nntp_proxy::tls::TlsConfig;
///
/// let config = TlsConfig::builder()
///     .enabled(true)
///     .verify_cert(false)
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct TlsConfigBuilder {
    use_tls: bool,
    tls_verify_cert: bool,
    tls_cert_path: Option<String>,
}

impl Default for TlsConfigBuilder {
    fn default() -> Self {
        Self {
            use_tls: false,
            tls_verify_cert: true, // Secure by default
            tls_cert_path: None,
        }
    }
}

impl TlsConfigBuilder {
    /// Enable or disable TLS
    ///
    /// Default: `false`
    pub fn enabled(mut self, use_tls: bool) -> Self {
        self.use_tls = use_tls;
        self
    }

    /// Enable or disable certificate verification
    ///
    /// **WARNING**: Disabling certificate verification is insecure and should only
    /// be used for testing or with trusted private networks.
    ///
    /// Default: `true`
    pub fn verify_cert(mut self, verify: bool) -> Self {
        self.tls_verify_cert = verify;
        self
    }

    /// Set path to custom CA certificate file
    ///
    /// The certificate should be in PEM format.
    pub fn cert_path<S: Into<String>>(mut self, path: S) -> Self {
        self.tls_cert_path = Some(path.into());
        self
    }

    /// Build the TlsConfig
    pub fn build(self) -> TlsConfig {
        TlsConfig {
            use_tls: self.use_tls,
            tls_verify_cert: self.tls_verify_cert,
            tls_cert_path: self.tls_cert_path,
        }
    }
}

// Certificate handling and custom verifiers

mod rustls_backend {
    use super::*;

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
    pub struct NoVerifier;

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

    /// Perform TLS handshake
    pub async fn handshake(
        &self,
        stream: TcpStream,
        hostname: &str,
        backend_name: &str,
    ) -> Result<TlsStream<TcpStream>, anyhow::Error> {
        use anyhow::Context;

        debug!("TLS: Connecting to {} with cached config", hostname);

        let domain = rustls_pki_types::ServerName::try_from(hostname)
            .context("Invalid hostname for TLS")?
            .to_owned();

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
    fn load_certificates_sync(
        config: &TlsConfig,
    ) -> Result<rustls_backend::CertificateLoadResult, anyhow::Error> {
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

        Ok(rustls_backend::CertificateLoadResult {
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
        use rustls_backend::NoVerifier;

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

        // Note: max_fragment_size is for outgoing records only (sending data to server)
        // For incoming data (server->client), rustls uses internal buffering
        // TLS 1.3 spec max is 16KB per record, larger values are not standard compliant
        // and can cause connection failures with some servers
        // client_config.max_fragment_size = Some(16384); // Keep default 16KB

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
    fn test_tls_config_builder_default() {
        let config = TlsConfig::builder().build();
        assert!(!config.use_tls);
        assert!(config.tls_verify_cert); // Secure by default
        assert!(config.tls_cert_path.is_none());
    }

    #[test]
    fn test_tls_config_builder_enabled() {
        let config = TlsConfig::builder().enabled(true).verify_cert(true).build();
        assert!(config.use_tls);
        assert!(config.tls_verify_cert);
        assert!(config.tls_cert_path.is_none());
    }

    #[test]
    fn test_tls_config_builder_with_cert_path() {
        let config = TlsConfig::builder()
            .enabled(true)
            .verify_cert(true)
            .cert_path("/path/to/cert.pem")
            .build();
        assert!(config.use_tls);
        assert!(config.tls_verify_cert);
        assert_eq!(config.tls_cert_path, Some("/path/to/cert.pem".to_string()));
    }

    #[test]
    fn test_tls_config_builder_insecure() {
        let config = TlsConfig::builder()
            .enabled(true)
            .verify_cert(false)
            .build();
        assert!(config.use_tls);
        assert!(!config.tls_verify_cert);
        assert!(config.tls_cert_path.is_none());
    }

    #[test]
    fn test_tls_config_builder_fluent_api() {
        let config = TlsConfig::builder()
            .enabled(true)
            .verify_cert(true)
            .cert_path("/custom/ca.pem".to_string())
            .build();
        assert!(config.use_tls);
        assert!(config.tls_verify_cert);
        assert_eq!(config.tls_cert_path, Some("/custom/ca.pem".to_string()));
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

    #[test]
    fn test_tls_config_builder_chaining() {
        let config = TlsConfig::builder()
            .enabled(false)
            .verify_cert(true)
            .enabled(true) // Override
            .build();

        assert!(config.use_tls);
        assert!(config.tls_verify_cert);
    }

    #[test]
    fn test_tls_config_clone() {
        let config1 = TlsConfig::builder()
            .enabled(true)
            .cert_path("/test/path")
            .build();

        let config2 = config1.clone();
        assert_eq!(config1.use_tls, config2.use_tls);
        assert_eq!(config1.tls_verify_cert, config2.tls_verify_cert);
        assert_eq!(config1.tls_cert_path, config2.tls_cert_path);
    }

    #[test]
    fn test_tls_manager_clone() {
        let config = TlsConfig::default();
        let manager1 = TlsManager::new(config).unwrap();
        let manager2 = manager1.clone();

        // Both should share the same Arc<TlsConnector>
        assert!(Arc::ptr_eq(
            &manager1.cached_connector,
            &manager2.cached_connector
        ));
    }

    #[test]
    fn test_tls_manager_debug() {
        let config = TlsConfig::default();
        let manager = TlsManager::new(config).unwrap();
        let debug_str = format!("{:?}", manager);

        assert!(debug_str.contains("TlsManager"));
        assert!(debug_str.contains("<TlsConnector>"));
    }

    #[test]
    fn test_tls_config_builder_cert_path_string_types() {
        // Test with &str
        let config1 = TlsConfig::builder().cert_path("/path/to/cert.pem").build();
        assert_eq!(config1.tls_cert_path, Some("/path/to/cert.pem".to_string()));

        // Test with String
        let config2 = TlsConfig::builder()
            .cert_path("/another/path.pem".to_string())
            .build();
        assert_eq!(config2.tls_cert_path, Some("/another/path.pem".to_string()));
    }

    #[test]
    fn test_no_verifier_supported_schemes() {
        use rustls_backend::NoVerifier;

        let verifier = NoVerifier;
        let schemes = verifier.supported_verify_schemes();

        // Should support all major signature schemes
        assert!(schemes.contains(&SignatureScheme::RSA_PKCS1_SHA256));
        assert!(schemes.contains(&SignatureScheme::ECDSA_NISTP256_SHA256));
        assert!(schemes.contains(&SignatureScheme::ED25519));
        assert!(schemes.len() >= 10); // Should have many schemes
    }

    #[test]
    fn test_certificate_load_result_sources() {
        let config = TlsConfig::default();
        let result = TlsManager::load_certificates_sync(&config).unwrap();

        // Should have at least one source
        assert!(!result.sources.is_empty());

        // Sources should be descriptive strings
        for source in &result.sources {
            assert!(!source.is_empty());
        }
    }

    #[test]
    fn test_tls_config_builder_defaults_are_secure() {
        // Builder should default to secure settings
        let config = TlsConfig::builder().enabled(true).build();

        assert!(config.use_tls);
        assert!(config.tls_verify_cert); // Verification enabled by default - SECURE
        assert!(config.tls_cert_path.is_none());
    }

    #[test]
    fn test_tls_manager_with_verify_disabled() {
        let config = TlsConfig::builder()
            .enabled(true)
            .verify_cert(false)
            .build();
        let manager = TlsManager::new(config);

        // Should successfully create manager even with verification disabled
        assert!(manager.is_ok());
    }

    #[test]
    fn test_tls_manager_with_verify_enabled() {
        let config = TlsConfig::builder().enabled(true).verify_cert(true).build();
        let manager = TlsManager::new(config);

        // Should successfully create manager with verification enabled
        assert!(manager.is_ok());
    }

    #[test]
    fn test_certificate_loading_fallback_to_mozilla_bundle() {
        // Even with empty config, should fall back to Mozilla CA bundle
        let config = TlsConfig::default();
        let result = TlsManager::load_certificates_sync(&config).unwrap();

        // Should have loaded certificates from some source
        assert!(!result.root_store.is_empty());
        assert!(!result.sources.is_empty());
    }

    #[test]
    fn test_tls_config_debug_format() {
        let config = TlsConfig::builder()
            .enabled(true)
            .verify_cert(false)
            .cert_path("/test")
            .build();

        let debug_str = format!("{:?}", config);

        assert!(debug_str.contains("TlsConfig"));
        assert!(debug_str.contains("use_tls"));
        assert!(debug_str.contains("tls_verify_cert"));
    }

    #[test]
    fn test_tls_config_builder_debug_format() {
        let builder = TlsConfig::builder().enabled(true).verify_cert(false);

        let debug_str = format!("{:?}", builder);

        assert!(debug_str.contains("TlsConfigBuilder"));
    }

    #[test]
    fn test_no_verifier_debug_format() {
        use rustls_backend::NoVerifier;

        let verifier = NoVerifier;
        let debug_str = format!("{:?}", verifier);

        assert!(debug_str.contains("NoVerifier"));
    }

    #[test]
    fn test_certificate_load_result_debug_format() {
        let config = TlsConfig::default();
        let result = TlsManager::load_certificates_sync(&config).unwrap();

        let debug_str = format!("{:?}", result);

        assert!(debug_str.contains("CertificateLoadResult"));
        assert!(debug_str.contains("root_store"));
        assert!(debug_str.contains("sources"));
    }

    #[test]
    fn test_tls_config_builder_cert_path_empty_string() {
        // Empty string is technically a valid path (current directory)
        let config = TlsConfig::builder().cert_path("").build();

        assert_eq!(config.tls_cert_path, Some(String::new()));
    }

    #[test]
    fn test_tls_config_builder_cert_path_with_spaces() {
        let config = TlsConfig::builder()
            .cert_path("  /path/with spaces.pem  ")
            .build();

        // Should preserve exact string including spaces
        assert_eq!(
            config.tls_cert_path,
            Some("  /path/with spaces.pem  ".to_string())
        );
    }

    #[test]
    fn test_multiple_tls_managers_from_same_config() {
        let config = TlsConfig::builder()
            .enabled(true)
            .verify_cert(false)
            .build();

        let manager1 = TlsManager::new(config.clone()).unwrap();
        let manager2 = TlsManager::new(config.clone()).unwrap();

        // Both should successfully initialize
        let debug1 = format!("{:?}", manager1);
        let debug2 = format!("{:?}", manager2);

        assert!(debug1.contains("TlsManager"));
        assert!(debug2.contains("TlsManager"));
    }

    #[test]
    fn test_tls_config_all_combinations() {
        // Test all boolean combinations
        for use_tls in [true, false] {
            for verify_cert in [true, false] {
                let config = TlsConfig::builder()
                    .enabled(use_tls)
                    .verify_cert(verify_cert)
                    .build();

                assert_eq!(config.use_tls, use_tls);
                assert_eq!(config.tls_verify_cert, verify_cert);

                // All configurations should create valid managers if TLS is enabled
                if use_tls {
                    let manager = TlsManager::new(config);
                    assert!(manager.is_ok());
                }
            }
        }
    }

    #[test]
    fn test_tls_config_builder_method_chaining_order() {
        // Test that builder methods can be called in any order
        let config1 = TlsConfig::builder()
            .enabled(true)
            .verify_cert(false)
            .cert_path("/test")
            .build();

        let config2 = TlsConfig::builder()
            .cert_path("/test")
            .verify_cert(false)
            .enabled(true)
            .build();

        assert_eq!(config1.use_tls, config2.use_tls);
        assert_eq!(config1.tls_verify_cert, config2.tls_verify_cert);
        assert_eq!(config1.tls_cert_path, config2.tls_cert_path);
    }

    #[test]
    fn test_tls_config_default_matches_builder_default() {
        let default_config = TlsConfig::default();
        let builder_config = TlsConfig::builder().build();

        assert_eq!(default_config.use_tls, builder_config.use_tls);
        assert_eq!(
            default_config.tls_verify_cert,
            builder_config.tls_verify_cert
        );
        assert_eq!(default_config.tls_cert_path, builder_config.tls_cert_path);
    }
}
