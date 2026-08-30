//! Bamep Worker process (Issue #37; Issue #39): the isolated, host-local OS
//! process `bamepd` supervises per ADR-0001/ADR-0003/ADR-0018. This library
//! crate holds the runtime pieces the `bamep-worker` binary (`src/main.rs`)
//! composes: configuration loading, Server TLS identity loading, the local
//! chunk byte-storage and full-Artifact reconstruction mechanisms
//! (`storage`), the concurrent reconnecting Worker Protocol v1 control client
//! (`ipc`), and the HTTPS `/api/data/v1/` data-plane listener that composes
//! them (`data_plane`).
//!
//! This crate has no dependency on `bamep-domain`, `bamep-server`, or
//! PostgreSQL/SQLx (ADR-0018 "Durable/business authority" — "Worker does
//! not own a PostgreSQL repository Adapter and does not independently
//! mutate Bamep durable Domain/Application state"). Worker owns no Domain/
//! Application authority here, only mechanism:
//!
//! - **`ipc`** — the reconnecting UDS control client. Issue #37 implemented
//!   the handshake / connection-generation / fail-closed-authority slice;
//!   Phase E1 completes it into the concurrent Worker Protocol v1 control
//!   client: many outstanding requests correlated by `message_id`/
//!   `in_reply_to`, generation-scoped follow-up tickets, a bounded per-request
//!   timeout, and every operation reporting only what `bamepd` decided (never
//!   a fabricated decision, never a local `Verified`/`Failed` verdict, never a
//!   replayed proof/handle across a reconnect).
//! - **`data_plane`** — the Worker-owned HTTPS `/api/data/v1/` listener
//!   (Axum 0.8 + `axum-server`, serving the same Server TLS identity the Agent
//!   already trusts). It implements the full operation set — resume discovery
//!   `GET`, chunk `PUT`, and seal `POST` — by composing `ipc` with `storage`;
//!   it performs structural HTTP parsing only and never verifies a capability,
//!   proof, or held-chunk truth, and never decides a `Verified`/`Failed`
//!   verdict.
//! - **`storage`** — a local chunk **byte-storage** mechanism (staging an
//!   authorized chunk body, hashing it incrementally with SHA-256, finalizing
//!   it into a restart-stable no-replace file) and **full-Artifact
//!   reconstruction** (reopening a sealed Artifact's finalized chunks in order
//!   and independently recomputing its full SHA-256). A finalized file never
//!   means `bamepd` accepted the chunk, and reconstruction reports only a
//!   mechanically computed digest.
//! - **`tls`** — loading the Server TLS identity that `data_plane` serves.
//!
//! Worker owns no durable/business authority (ADR-0018 "PostgreSQL and
//! storage": storage I/O, control transport, and HTTP serving are execution;
//! durable acceptance/sealing/verification are `bamepd`'s).

pub mod config;
pub mod data_plane;
pub mod ipc;
pub mod storage;
pub mod tls;
