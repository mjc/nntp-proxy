//! TLS configuration and handshake management for NNTP connections
//!
//! This module provides high-performance TLS support using rustls with optimizations:
//! - Ring crypto provider for fastest cryptographic operations
//! - TLS 1.3 early data (0-RTT) enabled for faster reconnections
//! - Session resumption enabled to avoid full handshakes
//! - Pure Rust implementation (memory safe, no C dependencies)
//! - System certificate loading with Mozilla CA bundle fallback

use crate::connection_error::ConnectionError;
use rustls::{ClientConfig, RootCertStore};
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

/// High-performance TLS connector with optimized configuration
pub struct TlsManager {
    config: TlsConfig,
}

impl TlsManager {
    /// Create a new TLS manager with the given configuration
    pub fn new(config: TlsConfig) -> Self {
        Self { config }
    }

    /// Perform TLS handshake with optimizations for maximum performance
    pub async fn handshake(
        &self,
        stream: TcpStream,
        hostname: &str,
        backend_name: &str,
    ) -> Result<TlsStream<TcpStream>, anyhow::Error> {
        let cert_result = self.load_certificates().await?;
        let client_config = self.create_optimized_config(cert_result.root_store)?;

        debug!(
            "TLS: Certificate sources: {}",
            cert_result.sources.join(", ")
        );

        let connector = TlsConnector::from(Arc::new(client_config));
        let domain = rustls_pki_types::ServerName::try_from(hostname)
            .map_err(|e| anyhow::anyhow!("Invalid hostname for TLS: {}", e))?
            .to_owned();

        debug!("TLS: Connecting to {} with rustls", hostname);
        connector.connect(domain, stream).await.map_err(|e| {
            ConnectionError::TlsHandshake {
                backend: backend_name.to_string(),
                source: Box::new(e),
            }
            .into()
        })
    }

    /// Load certificates from various sources with fallback chain
    async fn load_certificates(&self) -> Result<CertificateLoadResult, anyhow::Error> {
        let mut root_store = RootCertStore::empty();
        let mut sources = Vec::new();

        // 1. Load custom CA certificate if provided
        if let Some(cert_path) = &self.config.tls_cert_path {
            debug!("TLS: Loading custom CA certificate from: {}", cert_path);
            self.load_custom_certificate(&mut root_store, cert_path)?;
            sources.push("custom certificate".to_string());
        }

        // 2. Try to load system certificates
        let system_count = self.load_system_certificates(&mut root_store)?;
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
    fn load_custom_certificate(
        &self,
        root_store: &mut RootCertStore,
        cert_path: &str,
    ) -> Result<(), anyhow::Error> {
        let cert_data = std::fs::read(cert_path).map_err(|e| {
            anyhow::anyhow!("Failed to read TLS certificate from {}: {}", cert_path, e)
        })?;

        let certs = rustls_pemfile::certs(&mut cert_data.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Failed to parse TLS certificate: {}", e))?;

        for cert in certs {
            root_store
                .add(cert)
                .map_err(|e| anyhow::anyhow!("Failed to add custom certificate to store: {}", e))?;
        }

        Ok(())
    }

    /// Load system certificates, returning count of successfully loaded certificates
    fn load_system_certificates(
        &self,
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
    fn create_optimized_config(
        &self,
        root_store: RootCertStore,
    ) -> Result<ClientConfig, anyhow::Error> {
        let config_builder =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| {
                    anyhow::anyhow!("Failed to create TLS config with ring provider: {}", e)
                })?
                .with_root_certificates(root_store);

        let mut config = if self.config.tls_verify_cert {
            debug!("TLS: Certificate verification enabled with ring crypto provider");
            config_builder.with_no_client_auth()
        } else {
            debug!("TLS: WARNING - Certificate verification disabled (insecure!)");
            // For rustls, disabling cert verification requires a custom verifier
            // This is intentionally more difficult than native-tls for security
            return Err(anyhow::anyhow!(
                "Certificate verification cannot be disabled with rustls. \
                This is intentional for security. If you need to connect to \
                servers with invalid certificates, consider using a custom CA certificate."
            ));
        };

        // Performance optimizations
        config.enable_early_data = true; // Enable TLS 1.3 0-RTT for faster reconnections
        config.resumption = rustls::client::Resumption::default(); // Enable session resumption

        Ok(config)
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
        let _manager = TlsManager::new(config);
    }

    #[tokio::test]
    async fn test_certificate_loading() {
        let config = TlsConfig::default();
        let manager = TlsManager::new(config);

        let result = manager.load_certificates().await.unwrap();
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
