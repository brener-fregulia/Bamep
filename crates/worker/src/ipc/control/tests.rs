//! Phase E1 unit tests for the pure, non-I/O parts of the control client:
//! secret redaction in `Debug`, the response-type correlation table, and
//! `LocalGeneration`. The connection lifecycle (handshake, concurrency,
//! generation invalidation, timeout, saturation, resume aggregation, seal +
//! verification tickets, shutdown) is exercised against a real UDS peer in
//! `crates/worker/tests/control_client.rs`.

use super::*;

fn sample_auth_input() -> AuthorizeChunkInput {
    AuthorizeChunkInput {
        token: "super-secret-capability-token".to_string(),
        transfer_id: Uuid::new_v4(),
        chunk_index: 7,
        proof_id: "proof-id-value".to_string(),
        issued_at: 1_700_000_000_000,
        signature: "signature-bytes".to_string(),
    }
}

#[test]
fn authorize_chunk_input_debug_redacts_proof_material_but_keeps_correlation_identities() {
    let input = sample_auth_input();
    let rendered = format!("{input:?}");

    assert!(rendered.contains("chunk_index: 7"));
    assert!(rendered.contains(&input.transfer_id.to_string()));
    assert!(!rendered.contains("super-secret-capability-token"));
    assert!(!rendered.contains("proof-id-value"));
    assert!(!rendered.contains("signature-bytes"));
    assert!(!rendered.contains("1700000000000"));
    assert_eq!(rendered.matches("REDACTED").count(), 4);
}

#[test]
fn resume_and_seal_inputs_debug_redact_proof_material() {
    let resume = ResumeDiscoveryInput {
        token: "tok".to_string(),
        transfer_id: Uuid::new_v4(),
        proof_id: "pid".to_string(),
        issued_at: 42,
        signature: "sig".to_string(),
    };
    let rendered = format!("{resume:?}");
    assert!(
        !rendered.contains("\"tok\"")
            && !rendered.contains("\"pid\"")
            && !rendered.contains("\"sig\"")
    );

    let seal = ManifestSealInput {
        token: "tok2".to_string(),
        transfer_id: Uuid::new_v4(),
        proof_id: "pid2".to_string(),
        issued_at: 43,
        signature: "sig2".to_string(),
        chunk_count: 9,
        artifact_digest: "artifact-digest-text".to_string(),
    };
    let rendered = format!("{seal:?}");
    assert!(rendered.contains("chunk_count: 9"));
    assert!(
        rendered.contains("artifact-digest-text"),
        "the digest is an integrity identity, not a secret"
    );
    assert!(!rendered.contains("tok2") && !rendered.contains("pid2") && !rendered.contains("sig2"));
}

#[test]
fn tickets_debug_redacts_the_opaque_handle() {
    let acceptance = AcceptanceTicket {
        generation: LocalGeneration(3),
        handle: "acc_secret_handle_value".to_string(),
        transfer_id: Uuid::new_v4(),
        chunk_index: 1,
    };
    let rendered = format!("{acceptance:?}");
    assert!(rendered.contains("generation"));
    assert!(rendered.contains("chunk_index: 1"));
    assert!(!rendered.contains("acc_secret_handle_value"));

    let verification = VerificationTicket {
        generation: LocalGeneration(3),
        handle: "ver_secret_handle_value".to_string(),
    };
    assert!(!format!("{verification:?}").contains("ver_secret_handle_value"));
}

#[test]
fn expected_response_correlation_is_one_to_one() {
    use bamep_worker_protocol::WireArtifactStatus;

    let id = Uuid::new_v4();
    let authorization = ResponsePayload::Authorization(AuthorizationDecisionMessage::denied(id));
    let acceptance =
        ResponsePayload::ChunkAcceptance(ChunkAcceptanceDecisionMessage::committed(id));
    let page = ResponsePayload::ResumePage(ResumeDiscoveryPageMessage::denied(id));
    let seal = ResponsePayload::ManifestSeal(ManifestSealDecisionMessage::denied(id));
    let ack = ResponsePayload::ArtifactVerification(ArtifactVerificationAckMessage::committed(
        id,
        WireArtifactStatus::Verified,
    ));

    assert!(authorization.matches(ExpectedResponse::Authorization));
    assert!(!authorization.matches(ExpectedResponse::ChunkAcceptance));
    assert!(acceptance.matches(ExpectedResponse::ChunkAcceptance));
    assert!(!acceptance.matches(ExpectedResponse::Authorization));
    assert!(page.matches(ExpectedResponse::ResumePage));
    assert!(seal.matches(ExpectedResponse::ManifestSeal));
    assert!(ack.matches(ExpectedResponse::ArtifactVerification));
    assert!(!ack.matches(ExpectedResponse::ManifestSeal));
}

#[test]
fn local_generation_is_an_opaque_monotonic_wrapper() {
    assert_eq!(LocalGeneration(5).get(), 5);
    assert_ne!(LocalGeneration(1), LocalGeneration(2));
}

#[test]
fn control_errors_render_without_leaking_material() {
    // `ProtocolError` carries only bamepd's stable diagnostic code.
    let err = ControlError::ProtocolError {
        code: "unknown_message_type".to_string(),
    };
    assert!(err.to_string().contains("unknown_message_type"));

    let err = ControlError::Timeout { timeout_ms: 5000 };
    assert!(err.to_string().contains("5000"));
}
