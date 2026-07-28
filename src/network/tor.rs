use std::net::SocketAddr;
use std::time::Duration;

use log::info;

use crate::error::{NexusError, Result};

#[derive(Debug, Clone)]
pub struct TorConfig {
    pub enabled: bool,
    pub proxy_addr: SocketAddr,
    pub identity_rotation_minutes: u64,
    pub proxy_auth_username: Option<String>,
    pub proxy_auth_password: Option<String>,
    pub connect_timeout_seconds: u64,
}

impl Default for TorConfig {
    fn default() -> Self {
        TorConfig {
            enabled: false,
            proxy_addr: "127.0.0.1:9050".parse().expect("valid socket addr"),
            identity_rotation_minutes: 60,
            proxy_auth_username: None,
            proxy_auth_password: None,
            connect_timeout_seconds: 30,
        }
    }
}

/// Returns `true` if `host` (a bare hostname or a full URL) refers to a
/// Tor hidden service (`.onion`) address.
///
/// Extracts just the host portion before checking, rather than searching
/// the whole string for the substring `.onion` — a naive substring search
/// would incorrectly flag something like `http://example.onion.attacker.com`
/// (a normal clearnet domain that merely contains the substring, not an
/// actual `.onion` address) as a hidden service.
pub fn is_onion_address(host: &str) -> bool {
    let without_scheme = host
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host_only = without_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    host_only.to_lowercase().ends_with(".onion")
}

pub fn build_tor_client(config: &TorConfig) -> Result<reqwest::blocking::Client> {
    if !config.enabled {
        return Err(NexusError::Other("Tor is not enabled".to_string()));
    }

    let proxy_url = format!("socks5://{}", config.proxy_addr);
    let mut proxy = reqwest::Proxy::all(&proxy_url)
        .map_err(|e| NexusError::Other(format!("failed to create SOCKS5 proxy: {}", e)))?;

    if let (Some(user), Some(pass)) = (
        config.proxy_auth_username.as_ref(),
        config.proxy_auth_password.as_ref(),
    ) {
        proxy = proxy.basic_auth(user.as_str(), pass.as_str());
    }

    let timeout = Duration::from_secs(config.connect_timeout_seconds);

    let client = reqwest::blocking::Client::builder()
        .proxy(proxy)
        .timeout(timeout)
        .build()
        .map_err(|e| NexusError::Other(format!("failed to build Tor client: {}", e)))?;

    info!("Tor client configured with proxy {}", config.proxy_addr);

    Ok(client)
}

pub fn check_tor_reachable(config: &TorConfig) -> bool {
    if !config.enabled {
        return false;
    }

    match build_tor_client(config) {
        Ok(client) => match client.get("https://check.torproject.org/api/ip").send() {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                info!("Tor reachability check failed: {}", e);
                false
            }
        },
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_bare_onion_hosts() {
        assert!(is_onion_address("example.onion"));
        assert!(is_onion_address("EXAMPLE.ONION"));
        assert!(is_onion_address(
            "duskgytldkxiuqc6gzryzhcfr2h6bslixdsv9d4pd7lyat2xz2v7cyd.onion"
        ));
    }

    #[test]
    fn recognizes_full_urls_with_onion_host() {
        assert!(is_onion_address("http://example.onion/path?query=1"));
        assert!(is_onion_address("https://example.onion:8080/"));
    }

    #[test]
    fn rejects_clearnet_domains_that_merely_contain_onion_substring() {
        // Regression test: this used to false-positive because the old
        // implementation searched the whole string for ".onion" rather
        // than checking the actual host suffix.
        assert!(!is_onion_address("http://example.onion.attacker.com"));
        assert!(!is_onion_address("https://not-onion-related.com/onion-recipe"));
    }

    #[test]
    fn rejects_ordinary_domains() {
        assert!(!is_onion_address("example.com"));
        assert!(!is_onion_address("https://github.com"));
    }

    #[test]
    fn build_client_fails_when_tor_disabled() {
        let config = TorConfig {
            enabled: false,
            ..TorConfig::default()
        };
        assert!(build_tor_client(&config).is_err());
    }

    #[test]
    fn build_client_succeeds_when_enabled_with_valid_proxy_addr() {
        let config = TorConfig {
            enabled: true,
            ..TorConfig::default()
        };
        // Building the client just configures the SOCKS5 proxy settings;
        // it doesn't require an actual Tor daemon to be listening.
        assert!(build_tor_client(&config).is_ok());
    }

    #[test]
    fn check_reachable_returns_false_when_disabled_without_attempting_connection() {
        let config = TorConfig {
            enabled: false,
            ..TorConfig::default()
        };
        assert!(!check_tor_reachable(&config));
    }

    #[test]
    fn default_config_targets_standard_tor_port() {
        let config = TorConfig::default();
        assert_eq!(config.proxy_addr.port(), 9050);
        assert!(!config.enabled);
    }
}
