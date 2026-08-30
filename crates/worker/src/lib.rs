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
//! since Issue #39 Phase D1 — a local chunk **byte-storage** mechanism
//! (`storage`): staging an authorized chunk body, hashing it incrementally
//! with SHA-256, and finalizing it into a restart-stable file. That layer
//! still owns no durable or business authority; a finalized file never
//! means `bamepd` accepted the chunk (ADR-0018 "PostgreSQL and storage":
//! storage I/O is execution, durable acceptance is `bamepd`'s).
//!
//! Issue #37 implemented the handshake/connection-generation/fail-closed-
//! authority slice; Phase D1 adds `storage` only. Still out of scope here:
//! the business message catalog (`AuthorizationQuery`,
//! `ChunkAcceptanceRequest`, `ArtifactVerificationReport`, and their
//! responses), full-Artifact reconstruction across chunks, and the
//! Worker-owned HTTPS/TLS data-plane listener and routes — see
//! `bamep-worker-protocol` and this crate's `tls`/`storage` module docs.

pub mod config;
pub mod ipc;
pub mod storage;
pub mod tls;
