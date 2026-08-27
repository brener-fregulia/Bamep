//! Issue #38 correction §23 — independent Agent-side signer ↔ Server-side
//! verifier interoperability.
//!
//! The `bamep-simulator` proof builder ([`AgentTransferAuthorization`])
//! constructs and signs the 137-byte transcript entirely on its own; the
//! `bamep-domain` verifier independently *reconstructs* the transcript from
//! the same request facts (`bamep_domain::build_proof_transcript`) and checks
//! the signature (`bamep_domain::verify_proof_signature`). Neither side calls
//! the other's transcript builder — a byte-order, operation-code,
//! direction-code, timestamp-representation, capability-id, or
//! chunk-presence/index disagreement would make verification fail here.
//!
//! This is the real cross-implementation evidence the shared-code Domain unit
//! tests cannot give (they test one implementation against itself).

use bamep_domain::ArtifactId as DomainArtifactId;
use bamep_domain::{
    build_proof_transcript as domain_transcript, verify_proof_signature, AuthorizationOperation,
    CapabilityId, ProofId as DomainProofId, ProofPublicKey, ProofSignature, ProofTranscriptFields,
    TransferDirection as DomainDirection, TransferId as DomainTransferId,
};
use bamep_simulator::{
    AgentProofKey, AgentTransferAuthorization, DataPlaneTransferDirection, TransferOperation,
    TransferProof,
};
use uuid::Uuid;

const TOKEN: &str = "opaque-sender-constrained-capability-token-value";

fn agent_side(transfer_id: Uuid, artifact_id: Uuid) -> AgentTransferAuthorization {
    AgentTransferAuthorization::new(
        AgentProofKey::generate(),
        TOKEN,
        transfer_id,
        artifact_id,
        DataPlaneTransferDirection::AgentToServer,
        "https://server.example:8443",
    )
}

fn to_domain_op(op: TransferOperation) -> AuthorizationOperation {
    match op {
        TransferOperation::ChunkUpload => AuthorizationOperation::ChunkUpload,
        TransferOperation::ResumeDiscovery => AuthorizationOperation::ResumeDiscovery,
        TransferOperation::SealManifest => AuthorizationOperation::SealManifest,
    }
}

/// The Server side: reconstruct the transcript independently from request
/// context + wire proof material, then verify.
#[allow(clippy::too_many_arguments)]
fn server_verifies(
    proof_public_key_wire: &str,
    token: &str,
    operation: TransferOperation,
    transfer_id: Uuid,
    artifact_id: Uuid,
    chunk_index: Option<u64>,
    proof: &TransferProof,
) -> bool {
    let public_key = ProofPublicKey::parse_wire_value(proof_public_key_wire)
        .expect("Agent-side public key must parse under the strict Domain rule");
    let proof_id = DomainProofId::parse_wire_value(&proof.proof_id_wire)
        .expect("Agent-side proof_id must parse under the strict Domain rule");
    let signature = ProofSignature::parse_wire_value(&proof.signature_wire())
        .expect("Agent-side signature must parse under the strict Domain rule");
    let capability_id = CapabilityId::from_token_bytes(token.as_bytes());

    let transcript = domain_transcript(
        &capability_id,
        &ProofTranscriptFields {
            operation: to_domain_op(operation),
            transfer_id: DomainTransferId(transfer_id),
            artifact_id: DomainArtifactId(artifact_id),
            direction: DomainDirection::AgentToServer,
            chunk_index,
            proof_id,
            issued_at_millis: proof.issued_at_millis,
        },
    );
    verify_proof_signature(&public_key, &transcript, &signature)
}

#[test]
fn every_operation_and_chunk_identity_combination_verifies_across_implementations() {
    let cases: &[(TransferOperation, Option<u64>)] = &[
        (TransferOperation::ChunkUpload, Some(0)),
        (TransferOperation::ChunkUpload, Some(1)),
        (TransferOperation::ChunkUpload, Some(4_294_967_295)), // u32::MAX
        (TransferOperation::ResumeDiscovery, None),
        (TransferOperation::SealManifest, None),
    ];

    for &(operation, chunk_index) in cases {
        let transfer_id = Uuid::new_v4();
        let artifact_id = Uuid::new_v4();
        let agent = agent_side(transfer_id, artifact_id);
        let proof = agent
            .create_proof(operation, chunk_index, 1_700_000_123_456)
            .expect("valid operation/chunk pairing");

        assert!(
            server_verifies(
                &agent.proof_public_key_wire(),
                TOKEN,
                operation,
                transfer_id,
                artifact_id,
                chunk_index,
                &proof,
            ),
            "independent signer/verifier must agree for {operation:?} chunk={chunk_index:?}"
        );
    }
}

#[test]
fn a_mismatch_in_any_reconstructed_field_fails_verification() {
    let transfer_id = Uuid::new_v4();
    let artifact_id = Uuid::new_v4();
    let agent = agent_side(transfer_id, artifact_id);
    let pk = agent.proof_public_key_wire();
    let proof = agent
        .create_proof(TransferOperation::ChunkUpload, Some(7), 1_700_000_000_000)
        .unwrap();

    // Correct reconstruction verifies.
    assert!(server_verifies(
        &pk,
        TOKEN,
        TransferOperation::ChunkUpload,
        transfer_id,
        artifact_id,
        Some(7),
        &proof
    ));

    // Wrong operation code.
    assert!(!server_verifies(
        &pk,
        TOKEN,
        TransferOperation::SealManifest,
        transfer_id,
        artifact_id,
        Some(7),
        &proof
    ));
    // Wrong chunk index.
    assert!(!server_verifies(
        &pk,
        TOKEN,
        TransferOperation::ChunkUpload,
        transfer_id,
        artifact_id,
        Some(8),
        &proof
    ));
    // Wrong transfer_id.
    assert!(!server_verifies(
        &pk,
        TOKEN,
        TransferOperation::ChunkUpload,
        Uuid::new_v4(),
        artifact_id,
        Some(7),
        &proof
    ));
    // Wrong artifact_id.
    assert!(!server_verifies(
        &pk,
        TOKEN,
        TransferOperation::ChunkUpload,
        transfer_id,
        Uuid::new_v4(),
        Some(7),
        &proof
    ));
    // Wrong capability token (=> wrong capability_id in the transcript).
    assert!(!server_verifies(
        &pk,
        "a-different-token",
        TransferOperation::ChunkUpload,
        transfer_id,
        artifact_id,
        Some(7),
        &proof
    ));
}

#[test]
fn a_tampered_issued_at_breaks_verification() {
    let transfer_id = Uuid::new_v4();
    let artifact_id = Uuid::new_v4();
    let agent = agent_side(transfer_id, artifact_id);
    let pk = agent.proof_public_key_wire();
    let mut proof = agent
        .create_proof(TransferOperation::ResumeDiscovery, None, 1_700_000_000_000)
        .unwrap();

    assert!(server_verifies(
        &pk,
        TOKEN,
        TransferOperation::ResumeDiscovery,
        transfer_id,
        artifact_id,
        None,
        &proof
    ));

    // The transported `issued_at` no longer matches the signed one.
    proof.issued_at_millis += 1;
    assert!(!server_verifies(
        &pk,
        TOKEN,
        TransferOperation::ResumeDiscovery,
        transfer_id,
        artifact_id,
        None,
        &proof
    ));
}
