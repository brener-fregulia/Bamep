//! The `/api/data/v1/` router, common request parsing, fixed response
//! shapes, and the Phase E2A resume-discovery handler.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::ipc::{ResumeAggregate, ResumeDiscovery, ResumeDiscoveryInput, WorkerControlHandle};

/// `X-Bamep-Capability: <token>` (`m0-data-plane-and-storage-contracts.md`
/// "Common request elements").
const HEADER_CAPABILITY: &str = "x-bamep-capability";
/// `X-Bamep-Transfer-Proof: <proof_id>.<issued_at>.<signature>`.
const HEADER_TRANSFER_PROOF: &str = "x-bamep-transfer-proof";
/// `proof_id` is 22 base64url-no-pad characters (16 random bytes).
const PROOF_ID_LEN: usize = 22;
/// `signature` is 86 base64url-no-pad characters (a 64-byte Ed25519 signature).
const SIGNATURE_LEN: usize = 86;

#[derive(Clone)]
struct DataPlaneState {
    control: WorkerControlHandle,
}

/// Builds the exact `/api/data/v1/` router. Only the resume-discovery route
/// is registered in Phase E2A; every other path is a deterministic API
/// failure (never HTML, never a redirect).
pub fn router(control: WorkerControlHandle) -> Router {
    Router::new()
        .route(
            "/api/data/v1/transfers/{transfer_id}/chunks",
            get(resume_discovery),
        )
        // A recognised path with an unsupported method -> 405 (JSON).
        .method_not_allowed_fallback(method_not_allowed)
        // Any other path -> 404 UNKNOWN_ROUTE (JSON). Axum 0.8 performs no
        // trailing-slash normalisation, so `.../chunks/` lands here — never a
        // 3xx redirect.
        .fallback(unknown_route)
        .with_state(DataPlaneState { control })
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
        Err(control_error) => {
            // The contract maps every control-transport failure to the same
            // generic denial (non-enumerable). A safe internal line — no
            // secrets, no request material — aids operability.
            eprintln!(
                "bamep-worker: data-plane resume_discovery failed closed: control error class = {}",
                control_error_class(&control_error)
            );
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

// =====================================================================
// Fixed response shapes (reused by Phase E2B)
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
}
