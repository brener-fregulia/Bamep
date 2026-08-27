//! `bamepd` composition-root configuration (Issue #37 "Worker executable
//! discovery"/"Worker process config"): the minimum values needed to bind
//! the Worker UDS listener and supervise the Worker executable. A small
//! config struct plus environment parsing — no CLI framework — mirroring
//! `bamep_worker::config`.
//!
//! `BAMEP_WORKER_UDS_PATH`/`BAMEP_WORKER_TLS_CERT_PATH`/
//! `BAMEP_WORKER_TLS_KEY_PATH`/`BAMEP_WORKER_RECONNECT_DELAY_MS` are the
//! exact env var names `bamep_worker::config` reads on the Worker side —
//! duplicated here as literal constants rather than pulling in a
//! `bamep-server -> bamep-worker` crate dependency merely to share four
//! strings, which would blur the intentionally one-directional isolation
//! boundary (Issue #37 "Worker executable discovery"). [`BamepdConfig::worker_env`]
//! forwards them verbatim into the spawned Worker child's environment, so
//! both processes always agree on the same UDS path without a second name
//! to keep in sync. Keep these four literals identical to
//! `crates/worker/src/config.rs`'s `ENV_*` constants if either ever changes.

use std::path::PathBuf;
use std::time::Duration;

/// Must stay identical to `bamep_worker::config::ENV_UDS_PATH`.
pub const ENV_UDS_PATH: &str = "BAMEP_WORKER_UDS_PATH";
/// Must stay identical to `bamep_worker::config::ENV_TLS_CERT_PATH`.
pub const ENV_TLS_CERT_PATH: &str = "BAMEP_WORKER_TLS_CERT_PATH";
/// Must stay identical to `bamep_worker::config::ENV_TLS_KEY_PATH`.
pub const ENV_TLS_KEY_PATH: &str = "BAMEP_WORKER_TLS_KEY_PATH";
/// Must stay identical to `bamep_worker::config::ENV_RECONNECT_DELAY_MS`.
pub const ENV_RECONNECT_DELAY_MS: &str = "BAMEP_WORKER_RECONNECT_DELAY_MS";
pub const ENV_WORKER_EXECUTABLE: &str = "BAMEPD_WORKER_EXECUTABLE";
pub const ENV_WORKER_RESTART_DELAY_MS: &str = "BAMEPD_WORKER_RESTART_DELAY_MS";
/// PostgreSQL connection string (ADR-0013). Required starting with Issue
/// #38: the Worker control plane's `AuthorizationQuery` handler needs
/// current durable Transfer/Attempt/Endpoint-credential state to decide
/// authorization at all — `bamepd` can no longer avoid PostgreSQL startup
/// once that boundary is wired.
pub const ENV_DATABASE_URL: &str = "BAMEPD_DATABASE_URL";
/// The current Worker-owned data-plane HTTPS origin `bamepd` reports in
/// every `TransferAuthorizationGrant`
/// (`m0-agent-protocol-contract.md` "Endpoint discovery for the data-plane
/// listener"). Issue #38 only carries this value through; it binds no
/// listener of its own (#39).
pub const ENV_DATA_PLANE_BASE_URL: &str = "BAMEP_DATA_PLANE_BASE_URL";

const DEFAULT_RESTART_DELAY_MS: u64 = 500;
const DEFAULT_RECONNECT_DELAY_MS: u64 = 500;

/// Both the Worker restart delay and the reconnect delay forwarded to
/// Worker must be strictly positive: `0` would let a persistently failing
/// Worker spawn (`WorkerSupervisor::run`) or a persistently unreachable
/// `bamepd` (`bamep_worker::ipc::client::run_client_loop`) busy-loop
/// (correction audit "Bounded non-zero timing config").
///
/// `1`ms is not an operationally defensible floor (correction audit "Retry/
/// restart minimum"): at that scale the loop is still effectively
/// busy-retrying against a persistently failing Worker spawn/connection,
/// dominated by scheduler/syscall overhead rather than by the configured
/// delay. `100`ms is chosen as the smallest value that meaningfully bounds
/// retry frequency (at most 10 attempts/second) while staying far below the
/// `500`ms default, so an operator who deliberately configures a tighter
/// retry still gets one.
const MIN_DELAY_MS: u64 = 100;
/// Conservative implementation-time upper bound around the existing
/// `500ms` default, shared by both delays for consistency.
const MAX_DELAY_MS: u64 = 30_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BamepdConfigError {
    #[error("missing required environment variable {0}")]
    MissingEnv(&'static str),
    #[error("environment variable {name} has an invalid value: {reason}")]
    InvalidEnv { name: &'static str, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BamepdConfig {
    pub uds_path: PathBuf,
    pub worker_executable: PathBuf,
    pub tls_cert_path: PathBuf,
    pub tls_key_path: PathBuf,
    pub worker_restart_delay: Duration,
    pub worker_reconnect_delay: Duration,
    pub database_url: String,
    pub data_plane_base_url: String,
}

impl BamepdConfig {
    pub fn from_env() -> Result<Self, BamepdConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, BamepdConfigError> {
        let uds_path = required_path(&get, ENV_UDS_PATH)?;
        let worker_executable = required_path(&get, ENV_WORKER_EXECUTABLE)?;
        let tls_cert_path = required_path(&get, ENV_TLS_CERT_PATH)?;
        let tls_key_path = required_path(&get, ENV_TLS_KEY_PATH)?;
        let worker_restart_delay =
            optional_millis(&get, ENV_WORKER_RESTART_DELAY_MS, DEFAULT_RESTART_DELAY_MS)?;
        let worker_reconnect_delay =
            optional_millis(&get, ENV_RECONNECT_DELAY_MS, DEFAULT_RECONNECT_DELAY_MS)?;
        let database_url = required_string(&get, ENV_DATABASE_URL)?;
        let data_plane_base_url = required_https_origin(&get, ENV_DATA_PLANE_BASE_URL)?;

        Ok(Self {
            uds_path,
            worker_executable,
            tls_cert_path,
            tls_key_path,
            worker_restart_delay,
            worker_reconnect_delay,
            database_url,
            data_plane_base_url,
        })
    }

    /// The exact environment `bamepd` forwards to the spawned Worker child
    /// — UDS path and TLS identity paths/reconnect timing only, never
    /// business state (Issue #37 "Worker process config").
    pub fn worker_env(&self) -> Vec<(String, String)> {
        vec![
            (
                ENV_UDS_PATH.to_string(),
                self.uds_path.display().to_string(),
            ),
            (
                ENV_TLS_CERT_PATH.to_string(),
                self.tls_cert_path.display().to_string(),
            ),
            (
                ENV_TLS_KEY_PATH.to_string(),
                self.tls_key_path.display().to_string(),
            ),
            (
                ENV_RECONNECT_DELAY_MS.to_string(),
                self.worker_reconnect_delay.as_millis().to_string(),
            ),
        ]
    }
}

fn required_path(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<PathBuf, BamepdConfigError> {
    get(name)
        .map(PathBuf::from)
        .ok_or(BamepdConfigError::MissingEnv(name))
}

fn required_string(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, BamepdConfigError> {
    get(name).ok_or(BamepdConfigError::MissingEnv(name))
}

/// `data_plane_base_url` must be an HTTPS origin only — scheme, host, and
/// port, no path (`m0-agent-protocol-contract.md` "Endpoint discovery for
/// the data-plane listener"). This check is intentionally narrow: full URL
/// parsing is unnecessary for a value this Work Package only threads through
/// unmodified.
fn required_https_origin(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, BamepdConfigError> {
    let value = required_string(get, name)?;
    let Some(after_scheme) = value.strip_prefix("https://") else {
        return Err(BamepdConfigError::InvalidEnv {
            name,
            reason: "must be an https:// origin".to_string(),
        });
    };
    if after_scheme.is_empty() || after_scheme.contains('/') {
        return Err(BamepdConfigError::InvalidEnv {
            name,
            reason: "must carry no path — scheme, host, and port only".to_string(),
        });
    }
    Ok(value)
}

fn optional_millis(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default_ms: u64,
) -> Result<Duration, BamepdConfigError> {
    match get(name) {
        Some(raw) => {
            let ms: u64 = raw.parse().map_err(|_| BamepdConfigError::InvalidEnv {
                name,
                reason: "not a valid non-negative integer".to_string(),
            })?;
            if !(MIN_DELAY_MS..=MAX_DELAY_MS).contains(&ms) {
                return Err(BamepdConfigError::InvalidEnv {
                    name,
                    reason: format!(
                        "must be between {MIN_DELAY_MS} and {MAX_DELAY_MS} milliseconds, got {ms}"
                    ),
                });
            }
            Ok(Duration::from_millis(ms))
        }
        None => Ok(Duration::from_millis(default_ms)),
    }
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

    fn base_values() -> Vec<(&'static str, &'static str)> {
        vec![
            (ENV_UDS_PATH, "/run/bamep/worker.sock"),
            (ENV_WORKER_EXECUTABLE, "/opt/bamep/bin/bamep-worker"),
            (ENV_TLS_CERT_PATH, "/etc/bamep/tls/cert.pem"),
            (ENV_TLS_KEY_PATH, "/etc/bamep/tls/key.pem"),
            (ENV_DATABASE_URL, "postgres://bamep@localhost/bamep"),
            (ENV_DATA_PLANE_BASE_URL, "https://server.example:8443"),
        ]
    }

    #[test]
    fn loads_required_fields_with_defaults() {
        let config = BamepdConfig::from_lookup(lookup(&base_values())).expect("valid config");
        assert_eq!(config.uds_path, PathBuf::from("/run/bamep/worker.sock"));
        assert_eq!(
            config.worker_restart_delay,
            Duration::from_millis(DEFAULT_RESTART_DELAY_MS)
        );
        assert_eq!(
            config.worker_reconnect_delay,
            Duration::from_millis(DEFAULT_RECONNECT_DELAY_MS)
        );
        assert_eq!(config.database_url, "postgres://bamep@localhost/bamep");
        assert_eq!(config.data_plane_base_url, "https://server.example:8443");
    }

    #[test]
    fn missing_database_url_is_rejected() {
        let mut values = base_values();
        values.retain(|(name, _)| *name != ENV_DATABASE_URL);
        let err = BamepdConfig::from_lookup(lookup(&values)).unwrap_err();
        assert_eq!(err, BamepdConfigError::MissingEnv(ENV_DATABASE_URL));
    }

    #[test]
    fn a_non_https_data_plane_base_url_is_rejected() {
        let mut values = base_values();
        values.retain(|(name, _)| *name != ENV_DATA_PLANE_BASE_URL);
        values.push((ENV_DATA_PLANE_BASE_URL, "http://server.example:8443"));
        let err = BamepdConfig::from_lookup(lookup(&values)).unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_DATA_PLANE_BASE_URL)
        );
    }

    #[test]
    fn a_data_plane_base_url_carrying_a_path_is_rejected() {
        let mut values = base_values();
        values.retain(|(name, _)| *name != ENV_DATA_PLANE_BASE_URL);
        values.push((
            ENV_DATA_PLANE_BASE_URL,
            "https://server.example:8443/api/data/v1/",
        ));
        let err = BamepdConfig::from_lookup(lookup(&values)).unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_DATA_PLANE_BASE_URL)
        );
    }

    #[test]
    fn missing_required_variable_is_rejected() {
        let err = BamepdConfig::from_lookup(lookup(&[
            (ENV_WORKER_EXECUTABLE, "/opt/bamep/bin/bamep-worker"),
            (ENV_TLS_CERT_PATH, "/etc/bamep/tls/cert.pem"),
            (ENV_TLS_KEY_PATH, "/etc/bamep/tls/key.pem"),
        ]))
        .unwrap_err();
        assert_eq!(err, BamepdConfigError::MissingEnv(ENV_UDS_PATH));
    }

    #[test]
    fn worker_env_forwards_the_exact_variable_names_worker_config_reads() {
        let mut values = base_values();
        values.push((ENV_RECONNECT_DELAY_MS, "142"));
        let config = BamepdConfig::from_lookup(lookup(&values)).expect("valid config");
        let env = config.worker_env();

        let get = |name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        assert_eq!(
            get(ENV_UDS_PATH),
            Some("/run/bamep/worker.sock".to_string())
        );
        assert_eq!(
            get(ENV_TLS_CERT_PATH),
            Some("/etc/bamep/tls/cert.pem".to_string())
        );
        assert_eq!(
            get(ENV_TLS_KEY_PATH),
            Some("/etc/bamep/tls/key.pem".to_string())
        );
        assert_eq!(get(ENV_RECONNECT_DELAY_MS), Some("142".to_string()));
    }

    fn with_delay(name: &'static str, raw: &str) -> Result<BamepdConfig, BamepdConfigError> {
        let mut values = base_values();
        values.push((name, raw));
        BamepdConfig::from_lookup(lookup(&values))
    }

    #[test]
    fn zero_restart_delay_is_rejected() {
        let err = with_delay(ENV_WORKER_RESTART_DELAY_MS, "0").unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_WORKER_RESTART_DELAY_MS)
        );
    }

    #[test]
    fn minimum_restart_delay_is_accepted() {
        let config = with_delay(ENV_WORKER_RESTART_DELAY_MS, &MIN_DELAY_MS.to_string())
            .expect("valid config");
        assert_eq!(
            config.worker_restart_delay,
            Duration::from_millis(MIN_DELAY_MS)
        );
    }

    #[test]
    fn maximum_restart_delay_is_accepted() {
        let config = with_delay(ENV_WORKER_RESTART_DELAY_MS, &MAX_DELAY_MS.to_string())
            .expect("valid config");
        assert_eq!(
            config.worker_restart_delay,
            Duration::from_millis(MAX_DELAY_MS)
        );
    }

    #[test]
    fn above_maximum_restart_delay_is_rejected() {
        let err =
            with_delay(ENV_WORKER_RESTART_DELAY_MS, &(MAX_DELAY_MS + 1).to_string()).unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_WORKER_RESTART_DELAY_MS)
        );
    }

    #[test]
    fn malformed_restart_delay_is_rejected() {
        let err = with_delay(ENV_WORKER_RESTART_DELAY_MS, "not-a-number").unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_WORKER_RESTART_DELAY_MS)
        );
    }

    #[test]
    fn zero_reconnect_delay_is_rejected() {
        let err = with_delay(ENV_RECONNECT_DELAY_MS, "0").unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_RECONNECT_DELAY_MS)
        );
    }

    #[test]
    fn above_maximum_reconnect_delay_is_rejected() {
        let err = with_delay(ENV_RECONNECT_DELAY_MS, &(MAX_DELAY_MS + 1).to_string()).unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_RECONNECT_DELAY_MS)
        );
    }
}
