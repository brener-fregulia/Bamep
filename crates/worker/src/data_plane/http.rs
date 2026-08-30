//! The `/api/data/v1/` router, common request parsing, fixed response
//! shapes, and the operation handlers: resume discovery (Phase E2A), chunk
//! upload, and seal + verification (Phase E2B).

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::ipc::{ResumeAggregate, ResumeDiscovery, ResumeDiscoveryInput, WorkerControlHandle};
use crate::storage::FilesystemChunkStore;

#[cfg(unix)]
use super::upload;
#[cfg(unix)]
use crate::ipc::{
    ArtifactVerification, AuthorizeChunkInput, ChunkAcceptance, ChunkAuthorization, ManifestSeal,
    ManifestSealInput,
};
#[cfg(unix)]
use crate::storage::{FullArtifactError, FullArtifactHasher, FullArtifactRequest};
#[cfg(unix)]
use axum::body::Body;
#[cfg(unix)]
use axum::routing::{post, put};
#[cfg(unix)]
use bamep_worker_protocol::{ChunkAcceptanceRejectionReason, ManifestSealRejectionReason};

/// `X-Bamep-Capability: <token>` (`m0-data-plane-and-storage-contracts.md`
/// "Common request elements").
const HEADER_CAPABILITY: &str = "x-bamep-capability";
/// `X-Bamep-Transfer-Proof: <proof_id>.<issued_at>.<signature>`.
const HEADER_TRANSFER_PROOF: &str = "x-bamep-transfer-proof";
/// `proof_id` is 22 base64url-no-pad characters (16 random bytes).
const PROOF_ID_LEN: usize = 22;
/// `signature` is 86 base64url-no-pad characters (a 64-byte Ed25519 signature).
const SIGNATURE_LEN: usize = 86;
/// `X-Bamep-Chunk-Digest: <digest>` — the Agent-declared SHA-256 over the
/// exact `chunk_upload` request-body bytes, canonical base64url-no-pad.
#[cfg(unix)]
const HEADER_CHUNK_DIGEST: &str = "x-bamep-chunk-digest";
/// A base64url-no-pad SHA-256 digest is exactly 43 ASCII characters.
#[cfg(unix)]
const SHA256_DIGEST_B64_LEN: usize = 43;
/// Upper bound on the tiny `POST .../seal` JSON body (`{ "chunk_count",
/// "artifact_digest" }`). Anything larger is malformed, not a real seal.
#[cfg(unix)]
const SEAL_BODY_LIMIT: usize = 64 * 1024;

#[derive(Clone)]
struct DataPlaneState {
    control: WorkerControlHandle,
    /// Phase D1 storage: `chunk_upload` stages into it, `seal` reconstructs
    /// the full Artifact from it. Unused on non-Unix (the real store is
    /// Unix-only; the data plane there is a compile-only stub).
    #[cfg_attr(not(unix), allow(dead_code))]
    chunk_store: FilesystemChunkStore,
}

/// Builds the exact `/api/data/v1/` router. Every path outside the three
/// defined route shapes is a deterministic JSON API failure — never HTML,
/// never a 3xx redirect (Axum 0.8 performs no trailing-slash normalisation).
pub fn router(control: WorkerControlHandle, chunk_store: FilesystemChunkStore) -> Router {
    let state = DataPlaneState {
        control,
        chunk_store,
    };

    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut router = Router::new().route(
        "/api/data/v1/transfers/{transfer_id}/chunks",
        get(resume_discovery),
    );

    // Chunk `PUT` and seal `POST` compose the Unix-only D1/D2 storage
    // mechanism; the non-Unix build serves only resume discovery.
    #[cfg(unix)]
    {
        router = router
            .route(
                "/api/data/v1/transfers/{transfer_id}/chunks/{chunk_index}",
                put(chunk_upload),
            )
            .route(
                "/api/data/v1/transfers/{transfer_id}/seal",
                post(seal_manifest),
            );
    }

    router
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(unknown_route)
        .with_state(state)
}

// =====================================================================
// Resume discovery — GET /api/data/v1/transfers/{transfer_id}/chunks
// =====================================================================

async fn resume_discovery(
    State(state): State<DataPlaneState>,
    Path(transfer_id_raw): Path<String>,
    headers: HeaderMap,
) -> Response {
    // Structural parse only — never authorization.
    let transfer_id = match parse_canonical_uuid(&transfer_id_raw) {
        Some(id) => id,
        None => return malformed_request(),
    };
    let proof = match parse_common(&headers) {
        Ok(proof) => proof,
        Err(()) => return malformed_request(),
    };

    let input = ResumeDiscoveryInput {
        token: proof.token,
        transfer_id,
        proof_id: proof.proof_id,
        issued_at: proof.issued_at,
        signature: proof.signature,
    };

    match state.control.discover_resume(input).await {
        Ok(ResumeDiscovery::Approved(aggregate)) => render_resume(transfer_id, aggregate),
        Ok(ResumeDiscovery::Denied) => authorization_denied(),
        Err(error) => {
            // The contract maps every control-transport failure to the same
            // generic denial (non-enumerable).
            log_control_failure("resume_discovery", &error);
            authorization_denied()
        }
    }
}

fn render_resume(transfer_id: Uuid, aggregate: ResumeAggregate) -> Response {
    // Defense in depth against a malformed authoritative aggregate: strictly
    // ascending, unique `chunk_index`, and `expected_chunk_count` present iff
    // `sealed`. E1 / the protocol validators already guarantee both; an
    // impossible result fails closed rather than being re-sorted or rendered.
    let mut previous: Option<u64> = None;
    for chunk in &aggregate.held_chunks {
        if let Some(previous) = previous {
            if chunk.chunk_index <= previous {
                return authorization_denied();
            }
        }
        previous = Some(chunk.chunk_index);
    }
    if aggregate.sealed != aggregate.expected_chunk_count.is_some() {
        return authorization_denied();
    }

    let held: Vec<Value> = aggregate
        .held_chunks
        .iter()
        .map(|chunk| json!({ "chunk_index": chunk.chunk_index, "digest": chunk.digest }))
        .collect();

    let body = json!({
        // Canonical lowercase-hyphenated UUID, from the parsed route value.
        "transfer_id": transfer_id.to_string(),
        "sealed": aggregate.sealed,
        "digest_algorithm": digest_algorithm_wire(aggregate.digest_algorithm),
        "chunk_size": aggregate.chunk_size,
        // The HTTPS contract requires an explicit JSON `null` before sealing,
        // even though the UDS page omits the field.
        "expected_chunk_count": aggregate.expected_chunk_count,
        "held_chunks": held,
    });

    json_response(StatusCode::OK, body)
}

/// Exhaustive — a future `WireDigestAlgorithm` variant is a compile error
/// here, never a silent fall-through to `"sha256"`
/// (`m0-data-plane-and-storage-contracts.md` "Chunk manifest").
fn digest_algorithm_wire(algorithm: bamep_worker_protocol::WireDigestAlgorithm) -> &'static str {
    match algorithm {
        bamep_worker_protocol::WireDigestAlgorithm::Sha256 => "sha256",
    }
}

fn control_error_class(error: &crate::ipc::ControlError) -> &'static str {
    use crate::ipc::ControlError as E;
    match error {
        E::NotConnected => "not_connected",
        E::ConnectionLost => "connection_lost",
        E::GenerationChanged => "generation_changed",
        E::Timeout { .. } => "timeout",
        E::Saturated => "saturated",
        E::CorrelationViolation => "correlation_violation",
        E::ProtocolError { .. } => "protocol_error",
        E::ResumePageUnavailable => "resume_page_unavailable",
    }
}

/// A safe internal diagnostic line for a fail-closed control failure — no
/// secrets, no capability/proof/request material, just the error class.
fn log_control_failure(operation: &str, error: &crate::ipc::ControlError) {
    eprintln!(
        "bamep-worker: data-plane {operation} failed closed: control error class = {}",
        control_error_class(error)
    );
}

// =====================================================================
// Chunk upload — PUT /api/data/v1/transfers/{transfer_id}/chunks/{chunk_index}
// =====================================================================

/// Composition (`m0-data-plane-and-storage-contracts.md` "Durable chunk
/// acceptance ordering"): structural parse -> E1 `authorize_chunk` -> pre-body
/// rejections -> stream the body into D1 staging bounded by the authoritative
/// `chunk_size` -> mechanical SHA-256 -> validate the declared/expected digest
/// identity -> D1 restart-stable no-replace finalize -> E1 `commit_chunk` ->
/// only then the HTTP response. No `ChunkAcceptanceRequest` is sent before D1
/// finalization; no success is returned before `bamepd`'s durable commit.
#[cfg(unix)]
async fn chunk_upload(
    State(state): State<DataPlaneState>,
    Path((transfer_id_raw, chunk_index_raw)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    // ---- structural parse only (never authorization) ----
    let Some(transfer_id) = parse_canonical_uuid(&transfer_id_raw) else {
        return malformed_request();
    };
    let Some(chunk_index) = parse_canonical_chunk_index(&chunk_index_raw) else {
        return malformed_request();
    };
    let Ok(proof) = parse_common(&headers) else {
        return malformed_request();
    };
    let declared_digest = match header_str(&headers, HEADER_CHUNK_DIGEST) {
        Ok(value) if is_canonical_base64url_no_pad(value, SHA256_DIGEST_B64_LEN) => {
            value.to_string()
        }
        _ => return malformed_request(),
    };
    if !content_type_is_octet_stream(&headers) {
        return malformed_request();
    }

    // ---- E1 authorize BEFORE reading any body ----
    let input = AuthorizeChunkInput {
        token: proof.token,
        transfer_id,
        chunk_index,
        proof_id: proof.proof_id,
        issued_at: proof.issued_at,
        signature: proof.signature,
    };
    let approved = match state.control.authorize_chunk(input).await {
        Ok(ChunkAuthorization::Approved(approved)) => approved,
        Ok(ChunkAuthorization::Denied) => return authorization_denied(),
        Err(error) => {
            log_control_failure("chunk_upload authorize", &error);
            return authorization_denied();
        }
    };

    // Exhaustive: a future digest algorithm must not silently hash as SHA-256.
    match approved.digest_algorithm {
        bamep_worker_protocol::WireDigestAlgorithm::Sha256 => {}
    }

    // ---- pre-body rejections (step 3) ----
    if let Some(expected) = &approved.expected_chunk_digest {
        if *expected != declared_digest {
            return chunk_identity_conflict();
        }
    }
    if let Some(announced_len) = declared_content_length(&headers) {
        if announced_len > u64::from(approved.chunk_size) {
            return chunk_too_large();
        }
    }

    // ---- stream into D1, hash, validate, finalize (steps 4-5) ----
    let staged = upload::stage_chunk_body(
        state.chunk_store.clone(),
        upload::StageRequest {
            transfer_id,
            chunk_index,
            chunk_size: approved.chunk_size,
            declared_digest,
        },
        body,
    )
    .await;
    let (verified_digest, verified_size) = match staged {
        upload::StageOutcome::Finalized { size, digest } => (digest, size),
        // A zero-byte body cannot represent a `1..=chunk_size` chunk.
        upload::StageOutcome::EmptyBody => return malformed_request(),
        upload::StageOutcome::TooLarge => return chunk_too_large(),
        upload::StageOutcome::DigestMismatch => return digest_mismatch(),
        // Restart-stable local residue whose bytes differ from this upload —
        // a source-mutation transfer failure, fail closed (not an enumerable
        // `409`, which are `bamepd`-authoritative outcomes).
        upload::StageOutcome::LocalIdentityConflict | upload::StageOutcome::StorageUnavailable => {
            return authorization_denied();
        }
    };

    // ---- durable acceptance, then the HTTP response (steps 6-8) ----
    match state
        .control
        .commit_chunk(approved.acceptance_ticket, verified_digest, verified_size)
        .await
    {
        Ok(ChunkAcceptance::Committed) => chunk_accepted(chunk_index),
        Ok(ChunkAcceptance::AlreadyCommitted) => chunk_already_held(chunk_index),
        Ok(ChunkAcceptance::Rejected(ChunkAcceptanceRejectionReason::ChunkIdentityConflict)) => {
            chunk_identity_conflict()
        }
        Ok(ChunkAcceptance::Rejected(ChunkAcceptanceRejectionReason::TransferNotContinuable)) => {
            transfer_not_continuable()
        }
        Err(error) => {
            log_control_failure("chunk_upload commit", &error);
            authorization_denied()
        }
    }
}

// =====================================================================
// Seal + verify — POST /api/data/v1/transfers/{transfer_id}/seal
// =====================================================================

/// Composition: structural parse -> E1 `seal_manifest` (the first durable
/// `Incomplete -> PendingVerification` commit) -> D2 independent full-Artifact
/// reconstruction over the **authoritative** sealed `chunk_count`/`chunk_size`
/// (never the request body, never local file counts, never the expected
/// digest) -> E1 `report_artifact_verification`. `bamepd`'s authoritative
/// `Verified`/`Failed` — from its own comparison against the durable expected
/// digest — is the `200` response. Any failure after the first commit leaves
/// the Artifact `PendingVerification`; the request fails closed with the
/// generic `401` and a later idempotent seal retry re-drives verification.
#[cfg(unix)]
async fn seal_manifest(
    State(state): State<DataPlaneState>,
    Path(transfer_id_raw): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(transfer_id) = parse_canonical_uuid(&transfer_id_raw) else {
        return malformed_request();
    };
    let Ok(proof) = parse_common(&headers) else {
        return malformed_request();
    };
    let Some((chunk_count, artifact_digest)) = read_seal_body(body).await else {
        return malformed_request();
    };

    let input = ManifestSealInput {
        token: proof.token,
        transfer_id,
        proof_id: proof.proof_id,
        issued_at: proof.issued_at,
        signature: proof.signature,
        chunk_count,
        artifact_digest,
    };
    let success = match state.control.seal_manifest(input).await {
        Ok(ManifestSeal::Sealed(success))
        | Ok(ManifestSeal::AlreadyPendingVerification(success)) => success,
        Ok(ManifestSeal::Rejected(ManifestSealRejectionReason::IncompleteManifest)) => {
            return incomplete_manifest();
        }
        Ok(ManifestSeal::Rejected(ManifestSealRejectionReason::ManifestAlreadySealed)) => {
            return manifest_already_sealed();
        }
        Ok(ManifestSeal::Denied) => return authorization_denied(),
        Err(error) => {
            log_control_failure("seal_manifest seal", &error);
            return authorization_denied();
        }
    };

    // Exhaustive digest-algorithm guard (never fall through to SHA-256).
    match success.digest_algorithm {
        bamep_worker_protocol::WireDigestAlgorithm::Sha256 => {}
    }

    let request = FullArtifactRequest {
        transfer_id,
        chunk_count: success.chunk_count,
        chunk_size: success.chunk_size,
    };
    let computed = match FullArtifactHasher::for_store(&state.chunk_store)
        .compute_blocking(request)
        .await
    {
        Ok(full) => full.digest.to_base64url_no_pad(),
        Err(error) => {
            eprintln!(
                "bamep-worker: data-plane seal_manifest verification reread failed closed: {}",
                full_artifact_error_class(&error)
            );
            return authorization_denied();
        }
    };

    match state
        .control
        .report_artifact_verification(success.verification_ticket, computed)
        .await
    {
        Ok(status) => seal_result(transfer_id, success.artifact_id, status),
        Err(error) => {
            log_control_failure("seal_manifest verification report", &error);
            authorization_denied()
        }
    }
}

/// Reads and structurally validates the exact `POST .../seal` body:
/// `{ "chunk_count": <non-negative integer>, "artifact_digest": <canonical
/// base64url-no-pad SHA-256> }` and nothing else. Any deviation -> `None` ->
/// `400 MALFORMED_REQUEST`. The values themselves stay authoritative to
/// `bamepd` (the Worker verifies against the sealed values it returns).
#[cfg(unix)]
async fn read_seal_body(body: Body) -> Option<(u64, String)> {
    let bytes = axum::body::to_bytes(body, SEAL_BODY_LIMIT).await.ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let object = value.as_object()?;
    if object.len() != 2 {
        return None;
    }
    let chunk_count = object.get("chunk_count")?.as_u64()?;
    let artifact_digest = object.get("artifact_digest")?.as_str()?;
    if !is_canonical_base64url_no_pad(artifact_digest, SHA256_DIGEST_B64_LEN) {
        return None;
    }
    Some((chunk_count, artifact_digest.to_string()))
}

#[cfg(unix)]
fn seal_result(transfer_id: Uuid, artifact_id: Uuid, status: ArtifactVerification) -> Response {
    let artifact_status = match status {
        ArtifactVerification::Verified => "Verified",
        ArtifactVerification::Failed => "Failed",
    };
    json_response(
        StatusCode::OK,
        json!({
            "transfer_id": transfer_id.to_string(),
            "artifact_id": artifact_id.to_string(),
            "sealed": true,
            "artifact_status": artifact_status,
        }),
    )
}

#[cfg(unix)]
fn full_artifact_error_class(error: &FullArtifactError) -> &'static str {
    use FullArtifactError as E;
    match error {
        E::RequiredChunkMissing { .. } => "required_chunk_missing",
        E::RequiredChunkUnreadable { .. } => "required_chunk_unreadable",
        E::ChunkReadFailed { .. } => "chunk_read_failed",
        E::ChunkExceedsChunkSize { .. } => "chunk_exceeds_chunk_size",
        E::NonFinalChunkTooShort { .. } => "non_final_chunk_too_short",
        E::FinalChunkEmpty { .. } => "final_chunk_empty",
        E::TotalSizeOverflow { .. } => "total_size_overflow",
        E::BlockingTaskFailed => "blocking_task_failed",
    }
}

// =====================================================================
// Common request parsing (structural only)
// =====================================================================

struct CommonProof {
    token: String,
    proof_id: String,
    issued_at: u64,
    signature: String,
}

/// Parses `X-Bamep-Capability` + `X-Bamep-Transfer-Proof` into the fields E1
/// needs. `token` is forwarded byte-for-byte; the Worker never inspects its
/// structure. The proof carrier is validated only for the exact syntactic
/// shape the contract defines — `proof_id`/`signature` are canonical
/// base64url-no-pad of the fixed lengths, `issued_at` is a canonical decimal
/// integer — never cryptographically.
fn parse_common(headers: &HeaderMap) -> Result<CommonProof, ()> {
    let token = header_str(headers, HEADER_CAPABILITY)?;
    if token.is_empty() {
        return Err(());
    }

    let carrier = header_str(headers, HEADER_TRANSFER_PROOF)?;
    let mut segments = carrier.split('.');
    let (Some(proof_id), Some(issued_at_raw), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(());
    };

    if !is_canonical_base64url_no_pad(proof_id, PROOF_ID_LEN)
        || !is_canonical_base64url_no_pad(signature, SIGNATURE_LEN)
    {
        return Err(());
    }
    // "the decimal ASCII string of the exact Unix-millisecond integer" — a
    // canonical `u64` with no sign, whitespace, underscore, or leading zero.
    let issued_at: u64 = issued_at_raw.parse().map_err(|_| ())?;
    if issued_at_raw != issued_at.to_string() {
        return Err(());
    }

    Ok(CommonProof {
        token: token.to_string(),
        proof_id: proof_id.to_string(),
        issued_at,
        signature: signature.to_string(),
    })
}

fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> Result<&'h str, ()> {
    headers.get(name).ok_or(())?.to_str().map_err(|_| ())
}

/// Exact length + url-safe alphabet + no padding + a decode/re-encode
/// round-trip (rejects non-canonical trailing bits) — the "non-canonical
/// base64url in any header" -> `MALFORMED_REQUEST` rule.
fn is_canonical_base64url_no_pad(value: &str, expected_len: usize) -> bool {
    if value.len() != expected_len {
        return false;
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return false;
    }
    match URL_SAFE_NO_PAD.decode(value) {
        Ok(bytes) => URL_SAFE_NO_PAD.encode(bytes) == value,
        Err(_) => false,
    }
}

/// Accepts only the repository-canonical lowercase-hyphenated 36-character
/// UUID form; a well-formed-but-non-durable `transfer_id` is *not* rejected
/// here (that is an authorization denial `bamepd` decides), but an
/// unparseable / non-canonical string cannot be represented as an E1 request
/// at all.
fn parse_canonical_uuid(raw: &str) -> Option<Uuid> {
    let parsed = Uuid::parse_str(raw).ok()?;
    (parsed.hyphenated().to_string() == raw).then_some(parsed)
}

/// A `chunk_index` path segment in its one canonical form: ASCII digits only,
/// no sign, no `+`, no whitespace, no underscore, no leading zero (except
/// `"0"`), and within `u64` range. Anything else is "not a well-formed
/// non-negative integer" -> `MALFORMED_REQUEST`.
#[cfg(unix)]
fn parse_canonical_chunk_index(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parsed: u64 = raw.parse().ok()?;
    (parsed.to_string() == raw).then_some(parsed)
}

/// `Content-Type` must be `application/octet-stream` (a trailing `;`-parameter
/// is tolerated; the media type match is case-insensitive per RFC 9110).
#[cfg(unix)]
fn content_type_is_octet_stream(headers: &HeaderMap) -> bool {
    let Ok(value) = header_str(headers, header::CONTENT_TYPE.as_str()) else {
        return false;
    };
    value
        .split(';')
        .next()
        .map(|media_type| {
            media_type
                .trim()
                .eq_ignore_ascii_case("application/octet-stream")
        })
        .unwrap_or(false)
}

/// The declared body length, if a well-formed `Content-Length` is present.
/// Used only for an early `413` before the body is read; its absence (chunked
/// transfer) just defers the check to D1's streaming size bound.
#[cfg(unix)]
fn declared_content_length(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, header::CONTENT_LENGTH.as_str())
        .ok()?
        .parse()
        .ok()
}

// =====================================================================
// Fixed response shapes (shared by all operations)
// =====================================================================

fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        // Authorization-scoped dynamic transfer state — never cacheable.
        [(header::CACHE_CONTROL, "no-store")],
        Json(body),
    )
        .into_response()
}

fn error_response(status: StatusCode, code: &str) -> Response {
    json_response(status, json!({ "error": { "code": code } }))
}

/// The single fixed generic denial — exactly `{ "error": { "code":
/// "AUTHORIZATION_DENIED" } }`, no `message`, no variation — covering every
/// authorization failure *and* every control-transport failure
/// (`m0-data-plane-and-storage-contracts.md`; `m1-...` "Failure semantics").
pub(super) fn authorization_denied() -> Response {
    error_response(StatusCode::UNAUTHORIZED, "AUTHORIZATION_DENIED")
}

pub(super) fn malformed_request() -> Response {
    error_response(StatusCode::BAD_REQUEST, "MALFORMED_REQUEST")
}

async fn unknown_route() -> Response {
    error_response(StatusCode::NOT_FOUND, "UNKNOWN_ROUTE")
}

async fn method_not_allowed() -> Response {
    error_response(StatusCode::METHOD_NOT_ALLOWED, "METHOD_NOT_ALLOWED")
}

// ---- Phase E2B success + semantic-conflict shapes ----
// Exact status codes read from `m0-data-plane-and-storage-contracts.md`
// "Operations" (a `409` for every semantic conflict except an oversized
// body, which is `413`).

/// `201 Created` — `{ "chunk_index": N, "status": "accepted" }`.
#[cfg(unix)]
fn chunk_accepted(chunk_index: u64) -> Response {
    json_response(
        StatusCode::CREATED,
        json!({ "chunk_index": chunk_index, "status": "accepted" }),
    )
}

/// `200 OK` — `{ "chunk_index": N, "status": "already_held" }` for an
/// identical already-durable chunk resubmitted idempotently.
#[cfg(unix)]
fn chunk_already_held(chunk_index: u64) -> Response {
    json_response(
        StatusCode::OK,
        json!({ "chunk_index": chunk_index, "status": "already_held" }),
    )
}

#[cfg(unix)]
fn chunk_too_large() -> Response {
    error_response(StatusCode::PAYLOAD_TOO_LARGE, "CHUNK_TOO_LARGE")
}

#[cfg(unix)]
fn digest_mismatch() -> Response {
    error_response(StatusCode::CONFLICT, "DIGEST_MISMATCH")
}

#[cfg(unix)]
fn chunk_identity_conflict() -> Response {
    error_response(StatusCode::CONFLICT, "CHUNK_IDENTITY_CONFLICT")
}

#[cfg(unix)]
fn transfer_not_continuable() -> Response {
    error_response(StatusCode::CONFLICT, "TRANSFER_NOT_CONTINUABLE")
}

#[cfg(unix)]
fn incomplete_manifest() -> Response {
    error_response(StatusCode::CONFLICT, "INCOMPLETE_MANIFEST")
}

#[cfg(unix)]
fn manifest_already_sealed() -> Response {
    error_response(StatusCode::CONFLICT, "MANIFEST_ALREADY_SEALED")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    const PROOF_ID: &str = "AAAAAAAAAAAAAAAAAAAAAA"; // 22 chars, canonical (16 zero bytes)
    const SIGNATURE: &str =
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; // 86 chars

    fn valid_proof_carrier() -> String {
        format!("{PROOF_ID}.1700000000000.{SIGNATURE}")
    }

    #[test]
    fn parses_a_well_formed_common_request() {
        let parsed = parse_common(&headers(&[
            ("x-bamep-capability", "opaque-token-value"),
            ("x-bamep-transfer-proof", &valid_proof_carrier()),
        ]))
        .expect("valid");
        assert_eq!(parsed.token, "opaque-token-value");
        assert_eq!(parsed.proof_id, PROOF_ID);
        assert_eq!(parsed.issued_at, 1_700_000_000_000);
        assert_eq!(parsed.signature, SIGNATURE);
    }

    #[test]
    fn rejects_structurally_invalid_common_requests() {
        let carrier = valid_proof_carrier();
        let cases: &[&[(&str, &str)]] = &[
            &[("x-bamep-transfer-proof", "x")], // missing capability
            &[
                ("x-bamep-capability", ""),
                ("x-bamep-transfer-proof", "x.1.y"),
            ], // empty capability
            &[("x-bamep-capability", "t")],     // missing proof carrier
            &[
                ("x-bamep-capability", "t"),
                ("x-bamep-transfer-proof", "only.two"),
            ], // 2 segments
            &[
                ("x-bamep-capability", "t"),
                ("x-bamep-transfer-proof", "a.b.c.d"),
            ], // 4 segments
            &[
                ("x-bamep-capability", "t"),
                (
                    "x-bamep-transfer-proof",
                    &format!("{PROOF_ID}.01700000000000.{SIGNATURE}"),
                ),
            ], // leading-zero issued_at
            &[
                ("x-bamep-capability", "t"),
                (
                    "x-bamep-transfer-proof",
                    &format!("{PROOF_ID}.-1.{SIGNATURE}"),
                ),
            ], // signed issued_at
            &[
                ("x-bamep-capability", "t"),
                ("x-bamep-transfer-proof", &format!("shorty.1.{SIGNATURE}")),
            ], // proof_id wrong length
            &[
                ("x-bamep-capability", "t"),
                (
                    "x-bamep-transfer-proof",
                    &carrier.replace('A', "+"), // standard-alphabet char
                ),
            ],
        ];
        for case in cases {
            assert!(
                parse_common(&headers(case)).is_err(),
                "should reject {case:?}"
            );
        }
    }

    #[test]
    fn canonical_uuid_parsing_is_strict() {
        let id = Uuid::new_v4();
        let lower_hyphenated = id.hyphenated().to_string();
        assert_eq!(parse_canonical_uuid(&lower_hyphenated), Some(id));

        assert_eq!(parse_canonical_uuid(&lower_hyphenated.to_uppercase()), None);
        assert_eq!(parse_canonical_uuid(&id.simple().to_string()), None); // no hyphens
        assert_eq!(parse_canonical_uuid("not-a-uuid"), None);
        assert_eq!(parse_canonical_uuid(&id.urn().to_string()), None);
    }

    #[test]
    fn base64url_canonicality_check() {
        assert!(is_canonical_base64url_no_pad(PROOF_ID, 22));
        assert!(is_canonical_base64url_no_pad(SIGNATURE, 86));
        assert!(!is_canonical_base64url_no_pad("AAAA", 22)); // wrong length
                                                             // 22 chars, last char carries non-zero "leftover" bits -> decode ok,
                                                             // re-encode differs -> rejected.
        assert!(!is_canonical_base64url_no_pad("AAAAAAAAAAAAAAAAAAAAAB", 22));
        assert!(!is_canonical_base64url_no_pad(
            &format!("{}=", &PROOF_ID[..21]),
            22
        )); // padding
        assert!(!is_canonical_base64url_no_pad(&"/".repeat(22), 22)); // standard alphabet
    }

    #[test]
    fn digest_algorithm_maps_exhaustively() {
        assert_eq!(
            digest_algorithm_wire(bamep_worker_protocol::WireDigestAlgorithm::Sha256),
            "sha256"
        );
    }

    async fn read_response(response: Response) -> (StatusCode, Option<String>, Value) {
        let status = response.status();
        let cache = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, cache, serde_json::from_slice(&bytes).expect("json"))
    }

    #[tokio::test]
    async fn fixed_error_bodies_match_the_contract_exactly() {
        let (status, cache, body) = read_response(authorization_denied()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(cache.as_deref(), Some("no-store"));
        // EXACTLY this — no `message`, no other key.
        assert_eq!(body, json!({ "error": { "code": "AUTHORIZATION_DENIED" } }));

        let (status, _, body) = read_response(malformed_request()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({ "error": { "code": "MALFORMED_REQUEST" } }));

        let (status, _, body) = read_response(unknown_route().await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({ "error": { "code": "UNKNOWN_ROUTE" } }));

        let (status, _, body) = read_response(method_not_allowed().await).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body, json!({ "error": { "code": "METHOD_NOT_ALLOWED" } }));
    }

    #[cfg(unix)]
    #[test]
    fn chunk_index_parsing_is_canonical_decimal_only() {
        assert_eq!(parse_canonical_chunk_index("0"), Some(0));
        assert_eq!(parse_canonical_chunk_index("7"), Some(7));
        assert_eq!(
            parse_canonical_chunk_index("4294967296"),
            Some(4_294_967_296)
        );

        for bad in [
            "01", "007", "-1", "+1", " 1", "1 ", "1_0", "0x1", "", "1.0", "1e3",
        ] {
            assert_eq!(
                parse_canonical_chunk_index(bad),
                None,
                "must reject {bad:?}"
            );
        }
        // Overflows u64.
        assert_eq!(parse_canonical_chunk_index("18446744073709551616"), None);
    }

    #[cfg(unix)]
    #[test]
    fn content_type_must_be_octet_stream() {
        assert!(content_type_is_octet_stream(&headers(&[(
            "content-type",
            "application/octet-stream"
        )])));
        assert!(content_type_is_octet_stream(&headers(&[(
            "content-type",
            "Application/Octet-Stream"
        )])));
        assert!(content_type_is_octet_stream(&headers(&[(
            "content-type",
            "application/octet-stream; charset=binary"
        )])));
        assert!(!content_type_is_octet_stream(&headers(&[(
            "content-type",
            "application/json"
        )])));
        assert!(!content_type_is_octet_stream(&headers(&[])));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn e2b_success_and_conflict_bodies_match_the_contract_exactly() {
        let (status, cache, body) = read_response(chunk_accepted(3)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(cache.as_deref(), Some("no-store"));
        assert_eq!(body, json!({ "chunk_index": 3, "status": "accepted" }));

        let (status, _, body) = read_response(chunk_already_held(3)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "chunk_index": 3, "status": "already_held" }));

        for (response, expected_status, code) in [
            (
                chunk_too_large(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "CHUNK_TOO_LARGE",
            ),
            (digest_mismatch(), StatusCode::CONFLICT, "DIGEST_MISMATCH"),
            (
                chunk_identity_conflict(),
                StatusCode::CONFLICT,
                "CHUNK_IDENTITY_CONFLICT",
            ),
            (
                transfer_not_continuable(),
                StatusCode::CONFLICT,
                "TRANSFER_NOT_CONTINUABLE",
            ),
            (
                incomplete_manifest(),
                StatusCode::CONFLICT,
                "INCOMPLETE_MANIFEST",
            ),
            (
                manifest_already_sealed(),
                StatusCode::CONFLICT,
                "MANIFEST_ALREADY_SEALED",
            ),
        ] {
            let (status, _, body) = read_response(response).await;
            assert_eq!(status, expected_status, "{code}");
            assert_eq!(body, json!({ "error": { "code": code } }), "{code}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn seal_result_renders_the_exact_success_shape() {
        let transfer_id = Uuid::new_v4();
        let artifact_id = Uuid::new_v4();
        let (status, cache, body) = read_response(seal_result(
            transfer_id,
            artifact_id,
            ArtifactVerification::Failed,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cache.as_deref(), Some("no-store"));
        assert_eq!(
            body,
            json!({
                "transfer_id": transfer_id.to_string(),
                "artifact_id": artifact_id.to_string(),
                "sealed": true,
                "artifact_status": "Failed",
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_seal_body_accepts_exactly_the_two_contract_fields() {
        let digest = "A".repeat(43);
        let ok = axum::body::Body::from(
            json!({ "chunk_count": 5, "artifact_digest": digest }).to_string(),
        );
        assert_eq!(read_seal_body(ok).await, Some((5, digest.clone())));

        for bad in [
            "not json".to_string(),
            json!({ "chunk_count": 5 }).to_string(),
            json!({ "artifact_digest": digest }).to_string(),
            json!({ "chunk_count": -1, "artifact_digest": digest }).to_string(),
            json!({ "chunk_count": 5, "artifact_digest": "short" }).to_string(),
            json!({ "chunk_count": 5, "artifact_digest": digest, "x": 1 }).to_string(),
        ] {
            assert_eq!(
                read_seal_body(axum::body::Body::from(bad.clone())).await,
                None,
                "must reject {bad}"
            );
        }
    }
}
