//! Worker process configuration (Issue #37 "Worker process config"): the
//! minimum values needed to reach the `bamepd` UDS and load the Server TLS
//! identity. Deliberately carries no business state — no Transfer IDs,
//! capability tokens, proof keys, PostgreSQL URL, or other Domain state
//! (Issue #37 "Worker process config": "Do not send business state through
//! child startup args/environment").
//!
//! A small config struct plus environment parsing is sufficient here; no CLI
//! framework is introduced for four configuration values.

use std::path::PathBuf;
use std::time::Duration;

/// `bamepd` forwards this exact variable name in the child process
/// environment when it spawns Worker (`bamep_server::runtime::bamepd_config`),
/// so both processes name the same UDS path identically.
pub const ENV_UDS_PATH: &str = "BAMEP_WORKER_UDS_PATH";
pub const ENV_TLS_CERT_PATH: &str = "BAMEP_WORKER_TLS_CERT_PATH";
pub const ENV_TLS_KEY_PATH: &str = "BAMEP_WORKER_TLS_KEY_PATH";
pub const ENV_RECONNECT_DELAY_MS: &str = "BAMEP_WORKER_RECONNECT_DELAY_MS";

const DEFAULT_RECONNECT_DELAY_MS: u64 = 500;

/// The reconnect delay must be strictly positive: `0` would make the
/// reconnect loop (`crate::ipc::client::run_client_loop`) busy-spin against
/// a `bamepd` that is down or refusing connections (correction audit
/// "Bounded non-zero timing config").
///
/// `1`ms is not an operationally defensible floor (correction audit "Retry/
/// restart minimum"): kept identical to
/// `bamep_server::runtime::bamepd_config::MIN_DELAY_MS` (`100`ms) so both
/// sides of the Worker/`bamepd` reconnect relationship share the same
/// realistic retry-frequency floor.
const MIN_RECONNECT_DELAY_MS: u64 = 100;
/// Conservative implementation-time upper bound around the existing
/// `500ms` default: high enough to avoid meaningfully changing normal
/// operation, low enough that a misconfigured value cannot silently make
/// Worker take minutes to notice `bamepd` is back.
const MAX_RECONNECT_DELAY_MS: u64 = 30_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    MissingEnv(&'static str),
    #[error("environment variable {name} has an invalid value: {reason}")]
    InvalidEnv { name: &'static str, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerConfig {
    pub uds_path: PathBuf,
    pub tls_cert_path: PathBuf,
    pub tls_key_path: PathBuf,
    /// Bounded delay between reconnect attempts
    /// (`m1-worker-data-plane-control-contract.md` "Failure semantics":
    /// "Worker retries reconnect per its own backoff policy, which is
    /// implementation-time"). Fixed/bounded, not exponential, and always
    /// configurable so tests need not bake production timing into their
    /// assertions (Issue #37 "Restart policy"/"Worker reconnect").
    pub reconnect_delay: Duration,
}

impl WorkerConfig {
    /// Loads configuration from the real process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Testable indirection over environment lookup — avoids tests mutating
    /// real process-global environment variables under parallel execution.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let uds_path = required_path(&get, ENV_UDS_PATH)?;
        let tls_cert_path = required_path(&get, ENV_TLS_CERT_PATH)?;
        let tls_key_path = required_path(&get, ENV_TLS_KEY_PATH)?;
        let reconnect_delay = match get(ENV_RECONNECT_DELAY_MS) {
            Some(raw) => {
                let ms: u64 = raw.parse().map_err(|_| ConfigError::InvalidEnv {
                    name: ENV_RECONNECT_DELAY_MS,
                    reason: "not a valid non-negative integer".to_string(),
                })?;
                if !(MIN_RECONNECT_DELAY_MS..=MAX_RECONNECT_DELAY_MS).contains(&ms) {
                    return Err(ConfigError::InvalidEnv {
                        name: ENV_RECONNECT_DELAY_MS,
                        reason: format!(
                            "must be between {MIN_RECONNECT_DELAY_MS} and {MAX_RECONNECT_DELAY_MS} milliseconds, got {ms}"
                        ),
                    });
                }
                Duration::from_millis(ms)
            }
            None => Duration::from_millis(DEFAULT_RECONNECT_DELAY_MS),
        };

        Ok(Self {
            uds_path,
            tls_cert_path,
            tls_key_path,
            reconnect_delay,
        })
    }
}

fn required_path(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<PathBuf, ConfigError> {
    get(name)
        .map(PathBuf::from)
        .ok_or(ConfigError::MissingEnv(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn loads_required_fields_with_default_reconnect_delay() {
        let config = WorkerConfig::from_lookup(lookup(&[
            (ENV_UDS_PATH, "/run/bamep/worker.sock"),
            (ENV_TLS_CERT_PATH, "/etc/bamep/tls/cert.pem"),
            (ENV_TLS_KEY_PATH, "/etc/bamep/tls/key.pem"),
        ]))
        .expect("valid config");

        assert_eq!(config.uds_path, PathBuf::from("/run/bamep/worker.sock"));
        assert_eq!(
            config.reconnect_delay,
            Duration::from_millis(DEFAULT_RECONNECT_DELAY_MS)
        );
    }

    #[test]
    fn honors_explicit_reconnect_delay() {
        let config = WorkerConfig::from_lookup(lookup(&[
            (ENV_UDS_PATH, "/run/bamep/worker.sock"),
            (ENV_TLS_CERT_PATH, "/etc/bamep/tls/cert.pem"),
            (ENV_TLS_KEY_PATH, "/etc/bamep/tls/key.pem"),
            (ENV_RECONNECT_DELAY_MS, "150"),
        ]))
        .expect("valid config");

        assert_eq!(config.reconnect_delay, Duration::from_millis(150));
    }

    #[test]
    fn missing_required_variable_is_rejected() {
        let err = WorkerConfig::from_lookup(lookup(&[
            (ENV_TLS_CERT_PATH, "/etc/bamep/tls/cert.pem"),
            (ENV_TLS_KEY_PATH, "/etc/bamep/tls/key.pem"),
        ]))
        .unwrap_err();
        assert_eq!(err, ConfigError::MissingEnv(ENV_UDS_PATH));
    }

    #[test]
    fn invalid_reconnect_delay_is_rejected() {
        let err = WorkerConfig::from_lookup(lookup(&[
            (ENV_UDS_PATH, "/run/bamep/worker.sock"),
            (ENV_TLS_CERT_PATH, "/etc/bamep/tls/cert.pem"),
            (ENV_TLS_KEY_PATH, "/etc/bamep/tls/key.pem"),
            (ENV_RECONNECT_DELAY_MS, "not-a-number"),
        ]))
        .unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidEnv { name, .. } if name == ENV_RECONNECT_DELAY_MS)
        );
    }

    fn with_reconnect_delay(raw: &str) -> Result<WorkerConfig, ConfigError> {
        WorkerConfig::from_lookup(lookup(&[
            (ENV_UDS_PATH, "/run/bamep/worker.sock"),
            (ENV_TLS_CERT_PATH, "/etc/bamep/tls/cert.pem"),
            (ENV_TLS_KEY_PATH, "/etc/bamep/tls/key.pem"),
            (ENV_RECONNECT_DELAY_MS, raw),
        ]))
    }

    #[test]
    fn zero_reconnect_delay_is_rejected() {
        let err = with_reconnect_delay("0").unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidEnv { name, .. } if name == ENV_RECONNECT_DELAY_MS)
        );
    }

    #[test]
    fn minimum_reconnect_delay_is_accepted() {
        let config = with_reconnect_delay(&MIN_RECONNECT_DELAY_MS.to_string()).expect("valid");
        assert_eq!(
            config.reconnect_delay,
            Duration::from_millis(MIN_RECONNECT_DELAY_MS)
        );
    }

    #[test]
    fn maximum_reconnect_delay_is_accepted() {
        let config = with_reconnect_delay(&MAX_RECONNECT_DELAY_MS.to_string()).expect("valid");
        assert_eq!(
            config.reconnect_delay,
            Duration::from_millis(MAX_RECONNECT_DELAY_MS)
        );
    }

    #[test]
    fn above_maximum_reconnect_delay_is_rejected() {
        let err = with_reconnect_delay(&(MAX_RECONNECT_DELAY_MS + 1).to_string()).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidEnv { name, .. } if name == ENV_RECONNECT_DELAY_MS)
        );
    }
}
