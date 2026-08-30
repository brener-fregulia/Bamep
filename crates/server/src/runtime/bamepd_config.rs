//! `bamepd` composition-root configuration (Issue #37 "Worker executable
//! discovery"/"Worker process config"): the minimum values needed to bind
//! the Worker UDS listener and supervise the Worker executable. A small
//! config struct plus environment parsing — no CLI framework — mirroring
//! `bamep_worker::config`.
//!
//! `BAMEP_WORKER_UDS_PATH`/`BAMEP_WORKER_TLS_CERT_PATH`/
//! `BAMEP_WORKER_TLS_KEY_PATH`/`BAMEP_WORKER_RECONNECT_DELAY_MS`/
//! `BAMEP_WORKER_STORAGE_ROOT` are the exact env var names
//! `bamep_worker::config` reads on the Worker side — duplicated here as
//! literal constants rather than pulling in a `bamep-server -> bamep-worker`
//! crate dependency merely to share a handful of strings, which would blur
//! the intentionally one-directional isolation boundary (Issue #37 "Worker
//! executable discovery"). [`BamepdConfig::worker_env`] forwards them
//! verbatim into the spawned Worker child's environment, so both processes
//! always agree on the same paths without a second name to keep in sync.
//! Keep these literals identical to `crates/worker/src/config.rs`'s `ENV_*`
//! constants if either ever changes. All are filesystem locations / timing —
//! never Transfer IDs, capabilities, or proof material (ADR-0018).

use std::net::SocketAddr;
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
/// Must stay identical to `bamep_worker::config::ENV_STORAGE_ROOT`. The
/// absolute path to the Worker-owned local chunk storage tree (Issue #39
/// Phase D1); `bamepd` only carries it through to the child.
pub const ENV_STORAGE_ROOT: &str = "BAMEP_WORKER_STORAGE_ROOT";
/// Must stay identical to `bamep_worker::config::ENV_CONTROL_REQUEST_TIMEOUT_MS`.
/// Bounds how long one Worker Protocol control request waits for `bamepd`
/// (Issue #39 Phase E1); `bamepd` only carries it through to the child.
pub const ENV_CONTROL_REQUEST_TIMEOUT_MS: &str = "BAMEP_WORKER_CONTROL_REQUEST_TIMEOUT_MS";
/// Must stay identical to `bamep_worker::config::ENV_DATA_PLANE_BIND_ADDR`.
/// The local `IP:port` the Worker binds its HTTPS `/api/data/v1/` listener
/// to (Issue #39 Phase E2A). `bamepd` validates that its port equals the
/// port of `data_plane_base_url` before forwarding it — normal composition
/// cannot advertise one port and bind another (`m0-agent-protocol-contract.md`
/// "Endpoint discovery for the data-plane listener").
pub const ENV_DATA_PLANE_BIND_ADDR: &str = "BAMEP_WORKER_DATA_PLANE_BIND_ADDR";
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
/// `bamepd` (`bamep_worker::ipc::ControlDriver::run`) busy-loop
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

/// Control-request timeout bounds (Issue #39 Phase E1). `bamepd` only
/// carries this value through; the Worker enforces it. Kept identical to
/// `bamep_worker::config`'s own floor/default/ceiling.
const DEFAULT_CONTROL_REQUEST_TIMEOUT_MS: u64 = 5_000;
const MIN_CONTROL_REQUEST_TIMEOUT_MS: u64 = 250;
const MAX_CONTROL_REQUEST_TIMEOUT_MS: u64 = 60_000;

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
    /// Absolute path to the Worker-owned local chunk storage tree, forwarded
    /// verbatim to the spawned Worker child (Issue #39 Phase D1).
    /// `bamepd` does not read or validate this tree itself; the Worker's
    /// `crate::storage::FilesystemChunkStore::initialize` owns all checks.
    pub worker_storage_root: PathBuf,
    /// Bounded per-request answer deadline for the Worker's control client,
    /// forwarded verbatim to the spawned Worker child (Issue #39 Phase E1).
    /// `bamepd` neither reads nor enforces it — the Worker does.
    pub worker_control_request_timeout: Duration,
    /// Local `IP:port` the Worker binds its HTTPS data-plane listener to
    /// (Issue #39 Phase E2A), forwarded to the Worker child. Validated so its
    /// port equals `data_plane_base_url`'s port.
    pub worker_data_plane_bind_addr: SocketAddr,
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
        let worker_restart_delay = optional_millis(
            &get,
            ENV_WORKER_RESTART_DELAY_MS,
            DEFAULT_RESTART_DELAY_MS,
            MIN_DELAY_MS,
            MAX_DELAY_MS,
        )?;
        let worker_reconnect_delay = optional_millis(
            &get,
            ENV_RECONNECT_DELAY_MS,
            DEFAULT_RECONNECT_DELAY_MS,
            MIN_DELAY_MS,
            MAX_DELAY_MS,
        )?;
        let database_url = required_string(&get, ENV_DATABASE_URL)?;
        let data_plane_base_url = required_https_origin(&get, ENV_DATA_PLANE_BASE_URL)?;
        let worker_storage_root = required_path(&get, ENV_STORAGE_ROOT)?;
        let worker_control_request_timeout = optional_millis(
            &get,
            ENV_CONTROL_REQUEST_TIMEOUT_MS,
            DEFAULT_CONTROL_REQUEST_TIMEOUT_MS,
            MIN_CONTROL_REQUEST_TIMEOUT_MS,
            MAX_CONTROL_REQUEST_TIMEOUT_MS,
        )?;
        let worker_data_plane_bind_addr = required_bind_addr_matching_origin(
            &get,
            ENV_DATA_PLANE_BIND_ADDR,
            &data_plane_base_url,
        )?;

        Ok(Self {
            uds_path,
            worker_executable,
            tls_cert_path,
            tls_key_path,
            worker_restart_delay,
            worker_reconnect_delay,
            database_url,
            data_plane_base_url,
            worker_storage_root,
            worker_control_request_timeout,
            worker_data_plane_bind_addr,
        })
    }

    /// The exact environment `bamepd` forwards to the spawned Worker child
    /// — UDS path, TLS identity paths, reconnect/timeout timing, the chunk
    /// storage root, and the HTTPS bind address only, never business state
    /// (Issue #37 "Worker process config"; Phase D1 adds the storage root,
    /// Phase E1 the control-request timeout, Phase E2A the data-plane bind
    /// address — filesystem locations, durations, and a socket address).
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
            (
                ENV_STORAGE_ROOT.to_string(),
                self.worker_storage_root.display().to_string(),
            ),
            (
                ENV_CONTROL_REQUEST_TIMEOUT_MS.to_string(),
                self.worker_control_request_timeout.as_millis().to_string(),
            ),
            (
                ENV_DATA_PLANE_BIND_ADDR.to_string(),
                self.worker_data_plane_bind_addr.to_string(),
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

/// `data_plane_base_url` must be an HTTPS *origin* — exactly
/// `https://host[:port]` and nothing else (`m0-agent-protocol-contract.md`
/// "Endpoint discovery for the data-plane listener": "the HTTPS origin
/// (scheme, host, and port; no path)").
///
/// Validated through a real URI parser ([`url::Url`]) rather than
/// progressively accreted string checks — a prefix/`contains('/')` test
/// accepts malformed authorities, `userinfo@`, `?query`, and `#fragment`
/// shapes it was never meant to. Rejected: a non-`https` scheme, a missing
/// or empty host, any path segment (including a lone trailing `/`), a query,
/// a fragment, `username[:password]@` userinfo, a malformed port, and any
/// otherwise invalid authority. DNS, IPv4, and bracketed-IPv6 hosts are all
/// accepted; no particular port is required.
fn required_https_origin(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, BamepdConfigError> {
    let value = required_string(get, name)?;
    let reject = |reason: &str| BamepdConfigError::InvalidEnv {
        name,
        reason: reason.to_string(),
    };

    let url = url::Url::parse(&value).map_err(|e| reject(&format!("not a valid URI: {e}")))?;
    if url.scheme() != "https" {
        return Err(reject("must use the https scheme"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(reject("must not carry username/password userinfo"));
    }
    match url.host_str() {
        Some(host) if !host.is_empty() => {}
        _ => return Err(reject("must carry a host")),
    }
    if url.query().is_some() {
        return Err(reject("must not carry a query string"));
    }
    if url.fragment().is_some() {
        return Err(reject("must not carry a fragment"));
    }
    // The authority ends at the first `/` after `scheme://`; anything after
    // it — even a bare `/` — is a path segment this origin must not carry.
    let after_authority = value
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&value);
    if after_authority.contains('/') {
        return Err(reject("must carry no path — scheme, host, and port only"));
    }

    Ok(value)
}

/// Parses a required `IP:port` bind address for the Worker HTTPS listener
/// and rejects it unless its port equals the port of `data_plane_base_url`
/// (an explicit `https://` origin with no port means `443`). This is the
/// narrowest mechanism that makes it impossible for normal composition to
/// advertise `data_plane_base_url` on one port while the Worker binds another
/// (`m0-agent-protocol-contract.md` "Endpoint discovery for the data-plane
/// listener"). `bamepd` does not bind this address itself — it only forwards
/// it to the Worker child.
fn required_bind_addr_matching_origin(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    data_plane_base_url: &str,
) -> Result<SocketAddr, BamepdConfigError> {
    let raw = required_string(get, name)?;
    let bind_addr = raw
        .parse::<SocketAddr>()
        .map_err(|_| BamepdConfigError::InvalidEnv {
            name,
            reason: format!("must be an IP:port socket address, got {raw:?}"),
        })?;

    let origin_port = url::Url::parse(data_plane_base_url)
        .ok()
        .and_then(|url| url.port_or_known_default())
        .ok_or(BamepdConfigError::InvalidEnv {
            name,
            reason: "cannot determine the port of BAMEP_DATA_PLANE_BASE_URL".to_string(),
        })?;

    if bind_addr.port() != origin_port {
        return Err(BamepdConfigError::InvalidEnv {
            name,
            reason: format!(
                "port {} must equal the BAMEP_DATA_PLANE_BASE_URL port {origin_port}",
                bind_addr.port()
            ),
        });
    }
    Ok(bind_addr)
}

fn optional_millis(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default_ms: u64,
    min_ms: u64,
    max_ms: u64,
) -> Result<Duration, BamepdConfigError> {
    match get(name) {
        Some(raw) => {
            let ms: u64 = raw.parse().map_err(|_| BamepdConfigError::InvalidEnv {
                name,
                reason: "not a valid non-negative integer".to_string(),
            })?;
            if !(min_ms..=max_ms).contains(&ms) {
                return Err(BamepdConfigError::InvalidEnv {
                    name,
                    reason: format!("must be between {min_ms} and {max_ms} milliseconds, got {ms}"),
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
            (ENV_STORAGE_ROOT, "/var/lib/bamep/worker-chunks"),
            (ENV_DATA_PLANE_BIND_ADDR, "0.0.0.0:8443"),
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
        assert_eq!(
            config.worker_storage_root,
            PathBuf::from("/var/lib/bamep/worker-chunks")
        );
        assert_eq!(
            config.worker_control_request_timeout,
            Duration::from_millis(DEFAULT_CONTROL_REQUEST_TIMEOUT_MS)
        );
        assert_eq!(
            config.worker_data_plane_bind_addr,
            "0.0.0.0:8443".parse::<std::net::SocketAddr>().unwrap()
        );
    }

    #[test]
    fn the_data_plane_bind_addr_port_must_match_the_advertised_origin_port() {
        // Origin with an explicit port.
        let mut values = base_values();
        values.retain(|(name, _)| *name != ENV_DATA_PLANE_BIND_ADDR);
        values.push((ENV_DATA_PLANE_BIND_ADDR, "127.0.0.1:9999"));
        let err = BamepdConfig::from_lookup(lookup(&values)).unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_DATA_PLANE_BIND_ADDR)
        );

        // Origin with no explicit port defaults to 443.
        let mut values = base_values();
        values.retain(|(name, _)| {
            *name != ENV_DATA_PLANE_BASE_URL && *name != ENV_DATA_PLANE_BIND_ADDR
        });
        values.push((ENV_DATA_PLANE_BASE_URL, "https://server.example"));
        values.push((ENV_DATA_PLANE_BIND_ADDR, "0.0.0.0:443"));
        let config = BamepdConfig::from_lookup(lookup(&values)).expect("port 443 matches https");
        assert_eq!(config.worker_data_plane_bind_addr.port(), 443);

        let mut values = base_values();
        values.retain(|(name, _)| {
            *name != ENV_DATA_PLANE_BASE_URL && *name != ENV_DATA_PLANE_BIND_ADDR
        });
        values.push((ENV_DATA_PLANE_BASE_URL, "https://server.example"));
        values.push((ENV_DATA_PLANE_BIND_ADDR, "0.0.0.0:8443"));
        let err = BamepdConfig::from_lookup(lookup(&values)).unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_DATA_PLANE_BIND_ADDR)
        );
    }

    #[test]
    fn a_missing_or_malformed_data_plane_bind_addr_is_rejected() {
        let mut values = base_values();
        values.retain(|(name, _)| *name != ENV_DATA_PLANE_BIND_ADDR);
        assert_eq!(
            BamepdConfig::from_lookup(lookup(&values)).unwrap_err(),
            BamepdConfigError::MissingEnv(ENV_DATA_PLANE_BIND_ADDR)
        );

        let mut values = base_values();
        values.retain(|(name, _)| *name != ENV_DATA_PLANE_BIND_ADDR);
        values.push((ENV_DATA_PLANE_BIND_ADDR, "server.example:8443"));
        assert!(matches!(
            BamepdConfig::from_lookup(lookup(&values)).unwrap_err(),
            BamepdConfigError::InvalidEnv { name, .. } if name == ENV_DATA_PLANE_BIND_ADDR
        ));
    }

    #[test]
    fn honors_and_bounds_the_worker_control_request_timeout() {
        let config = with_delay(ENV_CONTROL_REQUEST_TIMEOUT_MS, "2000").expect("valid config");
        assert_eq!(
            config.worker_control_request_timeout,
            Duration::from_millis(2000)
        );

        let err = with_delay(
            ENV_CONTROL_REQUEST_TIMEOUT_MS,
            &(MIN_CONTROL_REQUEST_TIMEOUT_MS - 1).to_string(),
        )
        .unwrap_err();
        assert!(
            matches!(err, BamepdConfigError::InvalidEnv { name, .. } if name == ENV_CONTROL_REQUEST_TIMEOUT_MS)
        );
    }

    #[test]
    fn missing_database_url_is_rejected() {
        let mut values = base_values();
        values.retain(|(name, _)| *name != ENV_DATABASE_URL);
        let err = BamepdConfig::from_lookup(lookup(&values)).unwrap_err();
        assert_eq!(err, BamepdConfigError::MissingEnv(ENV_DATABASE_URL));
    }

    #[test]
    fn missing_worker_storage_root_is_rejected() {
        let mut values = base_values();
        values.retain(|(name, _)| *name != ENV_STORAGE_ROOT);
        let err = BamepdConfig::from_lookup(lookup(&values)).unwrap_err();
        assert_eq!(err, BamepdConfigError::MissingEnv(ENV_STORAGE_ROOT));
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

    fn origin_result(raw: &str) -> Result<BamepdConfig, BamepdConfigError> {
        let mut map: HashMap<String, String> = base_values()
            .into_iter()
            .filter(|(name, _)| *name != ENV_DATA_PLANE_BASE_URL)
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        map.insert(ENV_DATA_PLANE_BASE_URL.to_string(), raw.to_string());
        // Keep the bind-addr port consistent with a *well-formed* origin so
        // these tests isolate the origin-shape validation; an adversarial
        // origin fails on the origin check first, before the bind check.
        if let Some(port) = url::Url::parse(raw)
            .ok()
            .and_then(|url| url.port_or_known_default())
        {
            map.insert(
                ENV_DATA_PLANE_BIND_ADDR.to_string(),
                format!("0.0.0.0:{port}"),
            );
        }
        BamepdConfig::from_lookup(move |key: &str| map.get(key).cloned())
    }

    #[test]
    fn adversarial_data_plane_base_url_shapes_are_all_rejected() {
        for raw in [
            "http://server.example:8443",            // wrong scheme
            "ftp://server.example",                  // wrong scheme
            "https://",                              // missing host
            "https:///api",                          // empty authority
            "https://server.example:8443/",          // lone trailing-slash path
            "https://server.example:8443/api",       // path segment
            "https://server.example:8443?x=1",       // query
            "https://server.example:8443#frag",      // fragment
            "https://user:pass@server.example:8443", // userinfo
            "https://user@server.example",           // username-only userinfo
            "https://server.example:not-a-port",     // malformed port
            "https://server .example",               // invalid authority (space)
            "not-a-url",                             // not a URI at all
            "//server.example:8443",                 // scheme-relative
        ] {
            let outcome = origin_result(raw);
            assert!(
                matches!(&outcome, Err(BamepdConfigError::InvalidEnv { name, .. }) if *name == ENV_DATA_PLANE_BASE_URL),
                "{raw:?} must be rejected, got {outcome:?}"
            );
        }
    }

    #[test]
    fn valid_data_plane_origin_forms_are_accepted() {
        for raw in [
            "https://server.example:8443",
            "https://server.example",     // no explicit port
            "https://10.0.0.5:8443",      // IPv4 literal
            "https://[2001:db8::1]:8443", // bracketed IPv6 literal
        ] {
            let config = origin_result(raw)
                .unwrap_or_else(|e| panic!("{raw:?} must be accepted, got {e:?}"));
            assert_eq!(config.data_plane_base_url, raw);
        }
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
        assert_eq!(
            get(ENV_STORAGE_ROOT),
            Some("/var/lib/bamep/worker-chunks".to_string())
        );
        assert_eq!(
            get(ENV_CONTROL_REQUEST_TIMEOUT_MS),
            Some(DEFAULT_CONTROL_REQUEST_TIMEOUT_MS.to_string())
        );
        assert_eq!(
            get(ENV_DATA_PLANE_BIND_ADDR),
            Some("0.0.0.0:8443".to_string())
        );
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
