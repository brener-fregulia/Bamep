//! Bamep Worker process (Issue #37): the isolated, host-local OS process
//! `bamepd` supervises per ADR-0001/ADR-0003/ADR-0018. This library crate
//! holds the runtime pieces the `bamep-worker` binary (`src/main.rs`)
//! composes: configuration loading, Server TLS identity loading, and the
//! reconnecting UDS control-plane client.
//!
//! This crate has no dependency on `bamep-domain`, `bamep-server`, or
//! PostgreSQL/SQLx (ADR-0018 "Durable/business authority" — "Worker does
//! not own a PostgreSQL repository Adapter and does not independently
//! mutate Bamep durable Domain/Application state"). Worker owns no Domain/
//! Application authority here, only mechanism: loading its own TLS
//! identity, maintaining a fail-closed view of whether authoritative
//! `bamepd` control is currently available
//! (`docs/specifications/m1-worker-data-plane-control-contract.md`), and —
//! since Issue #39 Phase D1/D2 — the `storage` module: a local chunk
//! **byte-storage** mechanism (staging an authorized chunk body, hashing it
//! incrementally with SHA-256, finalizing it into a restart-stable file) and
//! **full-Artifact reconstruction** (reopening a sealed Artifact's finalized
//! chunks in order and independently recomputing its full SHA-256). That
//! layer still owns no durable or business authority: a finalized file never
//! means `bamepd` accepted the chunk, and reconstruction reports only a
//! mechanically computed digest — it decides no `Verified`/`Failed` verdict
//! (ADR-0018 "PostgreSQL and storage": storage I/O is execution, durable
//! acceptance/verification is `bamepd`'s).
//!
//! Issue #37 implemented the handshake/connection-generation/fail-closed-
//! authority slice; Phase D1/D2 add `storage` only. Still out of scope here:
//! the business message catalog (`AuthorizationQuery`,
//! `ChunkAcceptanceRequest`, `ManifestSealRequest`,
//! `ArtifactVerificationReport`, and their responses) and the Worker-owned
//! HTTPS/TLS data-plane listener and routes — see `bamep-worker-protocol`
//! and this crate's `tls`/`storage` module docs.

pub mod config;
pub mod ipc;
pub mod storage;
pub mod tls;
