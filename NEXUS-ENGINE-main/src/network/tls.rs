use crate::error::{NexusError, Result};

/// TLS policy configuration.
///
/// Note on current scope: this validates a *desired* TLS policy shape
/// (version floor, whether pinning is configured) but does not yet wire
/// certificate pinning into the actual `reqwest`/`rustls` connection
/// path — there's no client builder in this codebase that consults
/// `pinned_certificates` when establishing a connection. Treat this as
/// config validation for a policy that's declared but not yet enforced,
/// not as an active security control.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub min_tls_version: &'static str,
    pub enable_certificate_pinning: bool,
    pub pinned_certificates: Vec<String>,
    pub verify_hostnames: bool,
}

impl Default for TlsConfig {
    fn default() -> Self {
        TlsConfig {
            min_tls_version: "1.2",
            enable_certificate_pinning: false,
            pinned_certificates: Vec::new(),
            verify_hostnames: true,
        }
    }
}

pub fn verify_tls_config(config: &TlsConfig) -> Result<()> {
    match config.min_tls_version {
        "1.2" | "1.3" => {}
        other => {
            return Err(NexusError::Other(format!(
                "unsupported TLS version: {}. Use \"1.2\" or \"1.3\"",
                other
            )));
        }
    }

    if config.enable_certificate_pinning && config.pinned_certificates.is_empty() {
        return Err(NexusError::Other(
            "certificate pinning is enabled but no pinned certificates are configured".to_string(),
        ));
    }

    Ok(())
}

pub fn default_tls_version() -> &'static str {
    "1.2"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        assert!(verify_tls_config(&TlsConfig::default()).is_ok());
    }

    #[test]
    fn accepts_tls_1_2_and_1_3() {
        let mut config = TlsConfig::default();
        config.min_tls_version = "1.2";
        assert!(verify_tls_config(&config).is_ok());
        config.min_tls_version = "1.3";
        assert!(verify_tls_config(&config).is_ok());
    }

    #[test]
    fn rejects_unsupported_tls_versions() {
        let mut config = TlsConfig::default();
        config.min_tls_version = "1.0";
        assert!(verify_tls_config(&config).is_err());
        config.min_tls_version = "1.1";
        assert!(verify_tls_config(&config).is_err());
    }

    #[test]
    fn rejects_pinning_enabled_with_no_pinned_certs() {
        let config = TlsConfig {
            enable_certificate_pinning: true,
            pinned_certificates: Vec::new(),
            ..TlsConfig::default()
        };
        assert!(verify_tls_config(&config).is_err());
    }

    #[test]
    fn accepts_pinning_enabled_with_pinned_certs() {
        let config = TlsConfig {
            enable_certificate_pinning: true,
            pinned_certificates: vec!["sha256/AAAA...".to_string()],
            ..TlsConfig::default()
        };
        assert!(verify_tls_config(&config).is_ok());
    }
}
