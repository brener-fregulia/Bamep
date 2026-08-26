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
//! identity and maintaining a fail-closed view of whether authoritative
//! `bamepd` control is currently available
//! (`docs/specifications/m1-worker-data-plane-control-contract.md`).
//!
//! Issue #37 implements only the handshake/connection-generation/fail-
//! closed-authority slice. The business message catalog
//! (`AuthorizationQuery`, `ChunkAcceptanceRequest`,
//! `ArtifactVerificationReport`, and their responses) and the Worker-owned
//! HTTPS data-plane listener are out of scope — see `bamep-worker-protocol`
//! and this crate's `tls` module docs.

pub mod config;
pub mod ipc;
pub mod tls;
