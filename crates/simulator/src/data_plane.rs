//! Agent-side real HTTPS client for the Worker-owned `/api/data/v1/`
//! data-plane surface (`docs/specifications/m0-data-plane-and-storage-contracts.md`
//! "HTTPS data-plane v1 contract"; Issue #19 checkpoint C1).
//!
//! This module is a thin, typed HTTP consumer. It:
//!
//! - reuses the exact same trusted-leaf-pin TLS model the WSS control plane
//!   uses ([`crate::verifier::pinned_tls13_client_config`]; ADR-0018;
//!   `m0-agent-protocol-contract.md` "Endpoint discovery for the data-plane
//!   listener" — the data-plane origin introduces *no* new trust decision),
//!   never hostname/DNS/Web-PKI;
//! - targets the `data_plane_base_url` HTTPS origin exactly as delivered in
//!   `TransferAuthorizationGrant` — it never derives a host/port itself and
//!   hard-codes no `localhost`/`127.0.0.1`/port;
//! - speaks only the three exact v1 operations (`PUT` chunk, `GET` resume,
//!   `POST` seal) with the exact route/header/body/status contract;
//! - maps every response to a closed typed outcome, so the transfer state
//!   machine never matches raw JSON or HTTP status codes;
//! - depends on no `bamep-server` / `bamepd` internal error surface — only the
//!   public HTTPS response contract.
//!
//! Capability/proof semantics are **not** re-implemented here: the caller
//! supplies an already-obtained [`crate::transfer_authorization::AgentTransferAuthorization`]
//! and a freshly signed [`crate::transfer_authorization::TransferProof`] per
//! request. This module mints nothing and trusts nothing about the opaque
//! `token`.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{InvalidDnsNameError, ServerName};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use uuid::Uuid;

use crate::transfer_authorization::TransferProof;
use crate::verifier::pinned_tls13_client_config;
use bamep_trusted_bootstrap::ServerCertFingerprint;

/// `X-Bamep-Capability: <token>` (`m0-data-plane-and-storage-contracts.md`
/// "Common request elements").
const HEADER_CAPABILITY: &str = "x-bamep-capability";
/// `X-Bamep-Transfer-Proof: <proof_id>.<issued_at>.<signature>`.
const HEADER_TRANSFER_PROOF: &str = "x-bamep-transfer-proof";
/// `X-Bamep-Chunk-Digest: <digest>` — declared SHA-256 over the exact
/// `chunk_upload` body bytes, canonical base64url-no-pad.
const HEADER_CHUNK_DIGEST: &str = "x-bamep-chunk-digest";

/// Default per-request wall-clock ceiling. Overridable with
/// [`DataPlaneClient::with_request_timeout`]; the M1 vertical is deterministic
/// and small, so this only bounds a genuinely stuck connection.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A `data_plane_base_url` that is not a bare `https://host[:port]` origin, or
/// a TLS configuration failure. Fail-closed (`m0-data-plane-and-storage-contracts.md`;
/// Issue #19 C1 §9/§52) — the Agent never falls back to a derived origin.
#[derive(Debug, thiserror::Error)]
pub enum DataPlaneClientError {
    #[error(
        "data_plane_base_url is not a bare https://host[:port] origin (no userinfo, path, query, or fragment)"
    )]
    InvalidBaseUrl,
    #[error("the data-plane origin host is not a valid TLS server name")]
    ServerName(#[source] InvalidDnsNameError),
    #[error("failed to build the pinned TLS client configuration")]
    TlsConfig(#[source] rustls::Error),
}

/// A transport-level failure reaching or completing a data-plane request —
/// distinct from an HTTP-level contract outcome. The transfer runner maps this
/// to a resumable suspended state, never to terminal success.
#[derive(Debug, thiserror::Error)]
pub enum DataPlaneTransportError {
    #[error("TCP connect to the data-plane origin failed")]
    Connect(#[source] std::io::Error),
    #[error(
        "TLS handshake with the data-plane origin failed (pinned leaf verification, or peer TLS signature verification)"
    )]
    Tls(#[source] std::io::Error),
    #[error("HTTP/1.1 handshake with the data-plane origin failed")]
    HttpHandshake(#[source] hyper::Error),
    #[error("the data-plane request failed before a complete response")]
    Request(#[source] hyper::Error),
    #[error("the data-plane response body could not be read")]
    Body(#[source] hyper::Error),
    #[error("the data-plane request exceeded the configured timeout")]
    Timeout,
}

/// The authoritative committed Artifact status carried by a `200` seal
/// response (`m0-data-plane-and-storage-contracts.md` operation 3). `Failed`
/// is a completed operation reporting a `Failed` Artifact — still a `200`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealArtifactStatus {
    Verified,
    Failed,
}

/// Typed outcome of `PUT /api/data/v1/transfers/{transfer_id}/chunks/{chunk_index}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutChunkOutcome {
    /// `201` `{ "status": "accepted" }` — first-writer durable acceptance.
    Accepted { chunk_index: u64 },
    /// `200` `{ "status": "already_held" }` — identical already-durable chunk,
    /// idempotent.
    AlreadyHeld { chunk_index: u64 },
    /// `409 DIGEST_MISMATCH` — the bytes actually received hash to a value
    /// different from the declared/expected digest; never durably accepted.
    DigestMismatch,
    /// `409 CHUNK_IDENTITY_CONFLICT` — `chunk_index` is already durable with a
    /// *different* declared digest; the recorded identity is never rewritten.
    ChunkIdentityConflict,
    /// `409 TRANSFER_NOT_CONTINUABLE` — the owning Transfer/Artifact/Attempt is
    /// already terminal, or the manifest is sealed without this `chunk_index`.
    TransferNotContinuable,
    /// `413 CHUNK_TOO_LARGE` — the body exceeded the authoritative `chunk_size`.
    ChunkTooLarge,
    /// `401 AUTHORIZATION_DENIED` — the single fixed non-enumerable denial.
    AuthorizationDenied,
    /// `400 MALFORMED_REQUEST`.
    Malformed,
    /// Any status/body outside the operation's contract (`404`/`405`/`5xx`/an
    /// unrecognised `409` code/a `2xx` with the wrong body).
    Unexpected { status: u16 },
}

/// Typed outcome of `GET /api/data/v1/transfers/{transfer_id}/chunks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOutcome {
    Approved(ResumeManifest),
    AuthorizationDenied,
    Malformed,
    Unexpected { status: u16 },
}

/// One durably held + individually verified chunk identity from a resume
/// response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldChunk {
    pub chunk_index: u64,
    pub digest: String,
}

/// The aggregated resume-discovery state `bamepd` durably holds for one
/// Transfer (`m0-data-plane-and-storage-contracts.md` operation 2). Reflects
/// only durable, individually verified chunks — never Worker-staged bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeManifest {
    pub sealed: bool,
    pub digest_algorithm: String,
    pub chunk_size: u32,
    /// `null` before the manifest is sealed.
    pub expected_chunk_count: Option<u64>,
    pub held_chunks: Vec<HeldChunk>,
}

/// Typed outcome of `POST /api/data/v1/transfers/{transfer_id}/seal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealOutcome {
    /// `200` — sealing + full-Artifact verification completed. `artifact_status`
    /// is the authoritative durably-committed outcome.
    Completed {
        artifact_id: Uuid,
        artifact_status: SealArtifactStatus,
    },
    /// `409 INCOMPLETE_MANIFEST`.
    IncompleteManifest,
    /// `409 MANIFEST_ALREADY_SEALED` — already sealed with *different*
    /// `chunk_count`/`artifact_digest`.
    ManifestAlreadySealed,
    /// `401 AUTHORIZATION_DENIED`.
    AuthorizationDenied,
    /// `400 MALFORMED_REQUEST`.
    Malformed,
    Unexpected {
        status: u16,
    },
}

/// The Agent-side HTTPS client bound to one `data_plane_base_url` origin and
/// one trusted leaf fingerprint.
pub struct DataPlaneClient {
    origin: String,
    host: String,
    port: u16,
    sni: ServerName<'static>,
    tls: Arc<rustls::ClientConfig>,
    request_timeout: Duration,
}

impl std::fmt::Debug for DataPlaneClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlaneClient")
            .field("origin", &self.origin)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl DataPlaneClient {
    /// Binds to the exact `base_url` origin from `TransferAuthorizationGrant`,
    /// pinning the Server leaf to `expected` (the trusted-bootstrap
    /// `ServerCertFingerprint`). `base_url` MUST be a bare
    /// `https://host[:port]` origin; anything else fails closed.
    pub fn connect(
        base_url: &str,
        expected: ServerCertFingerprint,
    ) -> Result<Self, DataPlaneClientError> {
        let (host, port) = parse_https_origin(base_url)?;
        let sni = ServerName::try_from(host.clone()).map_err(DataPlaneClientError::ServerName)?;
        let tls = pinned_tls13_client_config(expected, vec![b"http/1.1".to_vec()])
            .map_err(DataPlaneClientError::TlsConfig)?;
        let authority = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        Ok(Self {
            origin: format!("https://{authority}"),
            host,
            port,
            sni,
            tls: Arc::new(tls),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        })
    }

    /// Overrides [`DEFAULT_REQUEST_TIMEOUT`].
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// The exact origin this client targets (for diagnostics/tests).
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// `GET /api/data/v1/transfers/{transfer_id}/chunks` — resume discovery.
    pub async fn discover_resume(
        &self,
        token: &str,
        transfer_id: Uuid,
        proof: &TransferProof,
    ) -> Result<ResumeOutcome, DataPlaneTransportError> {
        let path = format!("/api/data/v1/transfers/{transfer_id}/chunks");
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("{}{path}", self.origin))
            .header(HEADER_CAPABILITY, token)
            .header(HEADER_TRANSFER_PROOF, proof.header_value())
            .body(Full::new(Bytes::new()))
            .expect("static GET request is always well formed");
        let (status, body) = self.send(request).await?;
        Ok(parse_resume(status, &body))
    }

    /// `PUT /api/data/v1/transfers/{transfer_id}/chunks/{chunk_index}` — chunk
    /// upload. `chunk_digest_wire` is the Agent-declared digest of the *true*
    /// source bytes; `body` is what is actually transmitted (equal to the true
    /// bytes on a normal upload).
    pub async fn put_chunk(
        &self,
        token: &str,
        transfer_id: Uuid,
        chunk_index: u64,
        chunk_digest_wire: &str,
        proof: &TransferProof,
        body: Vec<u8>,
    ) -> Result<PutChunkOutcome, DataPlaneTransportError> {
        let path = format!("/api/data/v1/transfers/{transfer_id}/chunks/{chunk_index}");
        let request = Request::builder()
            .method(Method::PUT)
            .uri(format!("{}{path}", self.origin))
            .header(HEADER_CAPABILITY, token)
            .header(HEADER_TRANSFER_PROOF, proof.header_value())
            .header(HEADER_CHUNK_DIGEST, chunk_digest_wire)
            .header(hyper::header::CONTENT_TYPE, "application/octet-stream")
            .body(Full::new(Bytes::from(body)))
            .expect("chunk PUT request is always well formed");
        let (status, resp_body) = self.send(request).await?;
        Ok(parse_put(status, &resp_body, chunk_index))
    }

    /// `POST /api/data/v1/transfers/{transfer_id}/seal` — seal manifest and
    /// verify Artifact. `artifact_digest_wire` is the Agent's incrementally
    /// computed full-Artifact digest.
    pub async fn seal(
        &self,
        token: &str,
        transfer_id: Uuid,
        proof: &TransferProof,
        chunk_count: u64,
        artifact_digest_wire: &str,
    ) -> Result<SealOutcome, DataPlaneTransportError> {
        let path = format!("/api/data/v1/transfers/{transfer_id}/seal");
        let body = serde_json::json!({
            "chunk_count": chunk_count,
            "artifact_digest": artifact_digest_wire,
        })
        .to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("{}{path}", self.origin))
            .header(HEADER_CAPABILITY, token)
            .header(HEADER_TRANSFER_PROOF, proof.header_value())
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .expect("seal POST request is always well formed");
        let (status, resp_body) = self.send(request).await?;
        Ok(parse_seal(status, &resp_body))
    }

    /// Opens a fresh TCP -> pinned TLS 1.3 -> HTTP/1.1 connection, sends one
    /// request, and reads the full response. A fresh connection per request
    /// keeps the client trivially correct for the deterministic M1 vertical;
    /// pooling is not needed at C1 scale.
    async fn send(
        &self,
        request: Request<Full<Bytes>>,
    ) -> Result<(StatusCode, Bytes), DataPlaneTransportError> {
        let fut = self.send_inner(request);
        match tokio::time::timeout(self.request_timeout, fut).await {
            Ok(result) => result,
            Err(_) => Err(DataPlaneTransportError::Timeout),
        }
    }

    async fn send_inner(
        &self,
        request: Request<Full<Bytes>>,
    ) -> Result<(StatusCode, Bytes), DataPlaneTransportError> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(DataPlaneTransportError::Connect)?;
        let connector = TlsConnector::from(self.tls.clone());
        let tls = connector
            .connect(self.sni.clone(), tcp)
            .await
            .map_err(DataPlaneTransportError::Tls)?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(DataPlaneTransportError::HttpHandshake)?;
        let conn_task = tokio::spawn(async move {
            let _ = conn.await;
        });

        let response = sender
            .send_request(request)
            .await
            .map_err(DataPlaneTransportError::Request)?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(DataPlaneTransportError::Body)?
            .to_bytes();
        conn_task.abort();
        Ok((status, body))
    }
}

/// Parses a bare `https://host[:port]` origin. Rejects any userinfo, path
/// (beyond an empty one), query, or fragment — `bamepd` only ever grants this
/// exact shape (`m0-agent-protocol-contract.md` "Endpoint discovery"), and a
/// deviation is a fail-closed error rather than a best-effort parse.
fn parse_https_origin(base_url: &str) -> Result<(String, u16), DataPlaneClientError> {
    let uri: hyper::Uri = base_url
        .parse()
        .map_err(|_| DataPlaneClientError::InvalidBaseUrl)?;
    if uri.scheme_str() != Some("https") {
        return Err(DataPlaneClientError::InvalidBaseUrl);
    }
    if uri.query().is_some() {
        return Err(DataPlaneClientError::InvalidBaseUrl);
    }
    // `Uri` has no fragment, but a `#` in the input is a parse error above.
    let path = uri.path();
    if !path.is_empty() && path != "/" {
        return Err(DataPlaneClientError::InvalidBaseUrl);
    }
    let authority = uri
        .authority()
        .ok_or(DataPlaneClientError::InvalidBaseUrl)?;
    if authority.as_str().contains('@') {
        return Err(DataPlaneClientError::InvalidBaseUrl);
    }
    let host = authority.host();
    if host.is_empty() {
        return Err(DataPlaneClientError::InvalidBaseUrl);
    }
    let port = authority.port_u16().unwrap_or(443);
    Ok((host.to_string(), port))
}

fn parse_json(body: &[u8]) -> Option<Value> {
    serde_json::from_slice(body).ok()
}

fn error_code(value: &Value) -> Option<&str> {
    value.get("error")?.get("code")?.as_str()
}

fn parse_put(status: StatusCode, body: &[u8], chunk_index: u64) -> PutChunkOutcome {
    match status.as_u16() {
        201 => match parse_json(body).and_then(|v| chunk_status(&v)) {
            Some((status, idx)) if status == "accepted" && idx == chunk_index => {
                PutChunkOutcome::Accepted { chunk_index }
            }
            _ => PutChunkOutcome::Unexpected { status: 201 },
        },
        200 => match parse_json(body).and_then(|v| chunk_status(&v)) {
            Some((status, idx)) if status == "already_held" && idx == chunk_index => {
                PutChunkOutcome::AlreadyHeld { chunk_index }
            }
            _ => PutChunkOutcome::Unexpected { status: 200 },
        },
        400 => PutChunkOutcome::Malformed,
        401 => PutChunkOutcome::AuthorizationDenied,
        409 => match parse_json(body).as_ref().and_then(error_code) {
            Some("DIGEST_MISMATCH") => PutChunkOutcome::DigestMismatch,
            Some("CHUNK_IDENTITY_CONFLICT") => PutChunkOutcome::ChunkIdentityConflict,
            Some("TRANSFER_NOT_CONTINUABLE") => PutChunkOutcome::TransferNotContinuable,
            _ => PutChunkOutcome::Unexpected { status: 409 },
        },
        413 => match parse_json(body).as_ref().and_then(error_code) {
            Some("CHUNK_TOO_LARGE") => PutChunkOutcome::ChunkTooLarge,
            _ => PutChunkOutcome::Unexpected { status: 413 },
        },
        other => PutChunkOutcome::Unexpected { status: other },
    }
}

fn chunk_status(value: &Value) -> Option<(String, u64)> {
    let status = value.get("status")?.as_str()?.to_string();
    let index = value.get("chunk_index")?.as_u64()?;
    Some((status, index))
}

fn parse_resume(status: StatusCode, body: &[u8]) -> ResumeOutcome {
    match status.as_u16() {
        200 => match parse_json(body).and_then(|v| resume_manifest(&v)) {
            Some(manifest) => ResumeOutcome::Approved(manifest),
            None => ResumeOutcome::Unexpected { status: 200 },
        },
        400 => ResumeOutcome::Malformed,
        401 => ResumeOutcome::AuthorizationDenied,
        other => ResumeOutcome::Unexpected { status: other },
    }
}

fn resume_manifest(value: &Value) -> Option<ResumeManifest> {
    let object = value.as_object()?;
    let sealed = object.get("sealed")?.as_bool()?;
    let digest_algorithm = object.get("digest_algorithm")?.as_str()?.to_string();
    let chunk_size = u32::try_from(object.get("chunk_size")?.as_u64()?).ok()?;
    // Present but explicitly `null` before sealing.
    let expected_chunk_count = match object.get("expected_chunk_count")? {
        Value::Null => None,
        other => Some(other.as_u64()?),
    };
    let mut held_chunks = Vec::new();
    for entry in object.get("held_chunks")?.as_array()? {
        held_chunks.push(HeldChunk {
            chunk_index: entry.get("chunk_index")?.as_u64()?,
            digest: entry.get("digest")?.as_str()?.to_string(),
        });
    }
    Some(ResumeManifest {
        sealed,
        digest_algorithm,
        chunk_size,
        expected_chunk_count,
        held_chunks,
    })
}

fn parse_seal(status: StatusCode, body: &[u8]) -> SealOutcome {
    match status.as_u16() {
        200 => match parse_json(body).and_then(|v| seal_success(&v)) {
            Some((artifact_id, artifact_status)) => SealOutcome::Completed {
                artifact_id,
                artifact_status,
            },
            None => SealOutcome::Unexpected { status: 200 },
        },
        400 => SealOutcome::Malformed,
        401 => SealOutcome::AuthorizationDenied,
        409 => match parse_json(body).as_ref().and_then(error_code) {
            Some("INCOMPLETE_MANIFEST") => SealOutcome::IncompleteManifest,
            Some("MANIFEST_ALREADY_SEALED") => SealOutcome::ManifestAlreadySealed,
            _ => SealOutcome::Unexpected { status: 409 },
        },
        other => SealOutcome::Unexpected { status: other },
    }
}

fn seal_success(value: &Value) -> Option<(Uuid, SealArtifactStatus)> {
    let object = value.as_object()?;
    if !object.get("sealed")?.as_bool()? {
        return None;
    }
    let artifact_id = Uuid::parse_str(object.get("artifact_id")?.as_str()?).ok()?;
    let artifact_status = match object.get("artifact_status")?.as_str()? {
        "Verified" => SealArtifactStatus::Verified,
        "Failed" => SealArtifactStatus::Failed,
        _ => return None,
    };
    Some((artifact_id, artifact_status))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_https_origin_with_and_without_a_port() {
        assert_eq!(
            parse_https_origin("https://server.example:8443").unwrap(),
            ("server.example".to_string(), 8443)
        );
        assert_eq!(
            parse_https_origin("https://server.example").unwrap(),
            ("server.example".to_string(), 443)
        );
        assert_eq!(
            parse_https_origin("https://127.0.0.1:9000").unwrap(),
            ("127.0.0.1".to_string(), 9000)
        );
    }

    #[test]
    fn a_single_trailing_slash_is_the_one_tolerated_empty_path() {
        assert!(parse_https_origin("https://server.example:8443/").is_ok());
    }

    #[test]
    fn rejects_any_non_bare_origin() {
        for bad in [
            "http://server.example:8443",              // not https
            "https://server.example:8443/api/data/v1", // path
            "https://user@server.example:8443",        // userinfo
            "https://server.example:8443?x=1",         // query
            "wss://server.example:8443",               // wrong scheme
            "server.example:8443",                     // no scheme
            "https://",                                // no host
        ] {
            assert!(parse_https_origin(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn put_outcome_mapping_is_exact() {
        assert_eq!(
            parse_put(
                StatusCode::CREATED,
                br#"{"chunk_index":2,"status":"accepted"}"#,
                2
            ),
            PutChunkOutcome::Accepted { chunk_index: 2 }
        );
        assert_eq!(
            parse_put(
                StatusCode::OK,
                br#"{"chunk_index":2,"status":"already_held"}"#,
                2
            ),
            PutChunkOutcome::AlreadyHeld { chunk_index: 2 }
        );
        // wrong chunk_index in an otherwise-valid body is off-contract.
        assert_eq!(
            parse_put(
                StatusCode::CREATED,
                br#"{"chunk_index":9,"status":"accepted"}"#,
                2
            ),
            PutChunkOutcome::Unexpected { status: 201 }
        );
        assert_eq!(
            parse_put(
                StatusCode::UNAUTHORIZED,
                br#"{"error":{"code":"AUTHORIZATION_DENIED"}}"#,
                0
            ),
            PutChunkOutcome::AuthorizationDenied
        );
        assert_eq!(
            parse_put(
                StatusCode::CONFLICT,
                br#"{"error":{"code":"DIGEST_MISMATCH"}}"#,
                0
            ),
            PutChunkOutcome::DigestMismatch
        );
        assert_eq!(
            parse_put(
                StatusCode::CONFLICT,
                br#"{"error":{"code":"CHUNK_IDENTITY_CONFLICT"}}"#,
                0
            ),
            PutChunkOutcome::ChunkIdentityConflict
        );
        assert_eq!(
            parse_put(
                StatusCode::PAYLOAD_TOO_LARGE,
                br#"{"error":{"code":"CHUNK_TOO_LARGE"}}"#,
                0
            ),
            PutChunkOutcome::ChunkTooLarge
        );
        assert_eq!(
            parse_put(StatusCode::NOT_FOUND, b"", 0),
            PutChunkOutcome::Unexpected { status: 404 }
        );
        assert_eq!(
            parse_put(
                StatusCode::CONFLICT,
                br#"{"error":{"code":"SOMETHING_NEW"}}"#,
                0
            ),
            PutChunkOutcome::Unexpected { status: 409 }
        );
    }

    #[test]
    fn resume_manifest_parsing_handles_null_expected_count() {
        let unsealed = parse_resume(
            StatusCode::OK,
            br#"{"transfer_id":"x","sealed":false,"digest_algorithm":"sha256","chunk_size":4096,"expected_chunk_count":null,"held_chunks":[]}"#,
        );
        assert_eq!(
            unsealed,
            ResumeOutcome::Approved(ResumeManifest {
                sealed: false,
                digest_algorithm: "sha256".to_string(),
                chunk_size: 4096,
                expected_chunk_count: None,
                held_chunks: vec![],
            })
        );
        let sealed = parse_resume(
            StatusCode::OK,
            br#"{"sealed":true,"digest_algorithm":"sha256","chunk_size":4096,"expected_chunk_count":3,"held_chunks":[{"chunk_index":0,"digest":"abc"},{"chunk_index":2,"digest":"def"}]}"#,
        );
        assert_eq!(
            sealed,
            ResumeOutcome::Approved(ResumeManifest {
                sealed: true,
                digest_algorithm: "sha256".to_string(),
                chunk_size: 4096,
                expected_chunk_count: Some(3),
                held_chunks: vec![
                    HeldChunk {
                        chunk_index: 0,
                        digest: "abc".to_string()
                    },
                    HeldChunk {
                        chunk_index: 2,
                        digest: "def".to_string()
                    },
                ],
            })
        );
        assert_eq!(
            parse_resume(
                StatusCode::UNAUTHORIZED,
                br#"{"error":{"code":"AUTHORIZATION_DENIED"}}"#
            ),
            ResumeOutcome::AuthorizationDenied
        );
    }

    #[test]
    fn seal_outcome_mapping_is_exact() {
        assert_eq!(
            parse_seal(
                StatusCode::OK,
                br#"{"transfer_id":"t","artifact_id":"550e8400-e29b-41d4-a716-446655440000","sealed":true,"artifact_status":"Verified"}"#,
            ),
            SealOutcome::Completed {
                artifact_id: Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
                artifact_status: SealArtifactStatus::Verified,
            }
        );
        assert_eq!(
            parse_seal(
                StatusCode::OK,
                br#"{"transfer_id":"t","artifact_id":"550e8400-e29b-41d4-a716-446655440000","sealed":true,"artifact_status":"Failed"}"#,
            ),
            SealOutcome::Completed {
                artifact_id: Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap(),
                artifact_status: SealArtifactStatus::Failed,
            }
        );
        assert_eq!(
            parse_seal(
                StatusCode::CONFLICT,
                br#"{"error":{"code":"INCOMPLETE_MANIFEST"}}"#
            ),
            SealOutcome::IncompleteManifest
        );
        assert_eq!(
            parse_seal(
                StatusCode::CONFLICT,
                br#"{"error":{"code":"MANIFEST_ALREADY_SEALED"}}"#
            ),
            SealOutcome::ManifestAlreadySealed
        );
        assert_eq!(
            parse_seal(
                StatusCode::UNAUTHORIZED,
                br#"{"error":{"code":"AUTHORIZATION_DENIED"}}"#
            ),
            SealOutcome::AuthorizationDenied
        );
    }
}
