//! Bamep Worker process (Issue #37; Issue #39 Phases D1/D2/E1): the isolated,
//! host-local OS process `bamepd` supervises per ADR-0001/ADR-0003/ADR-0018.
//! This library crate holds the runtime pieces the `bamep-worker` binary
//! (`src/main.rs`) composes: configuration loading, Server TLS identity
//! loading, the local chunk byte-storage and full-Artifact reconstruction
//! mechanisms (`storage`), and the concurrent, reconnecting Worker Protocol
//! v1 control client (`ipc`).
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
//! - **`storage`** — since Phase D1/D2: a local chunk **byte-storage**
//!   mechanism (staging an authorized chunk body, hashing it incrementally
//!   with SHA-256, finalizing it into a restart-stable file) and
//!   **full-Artifact reconstruction** (reopening a sealed Artifact's finalized
//!   chunks in order and independently recomputing its full SHA-256). A
//!   finalized file never means `bamepd` accepted the chunk, and
//!   reconstruction reports only a mechanically computed digest.
//! - **`tls`** — loading the Server TLS identity (not yet bound to a
//!   listener).
//!
//! Worker owns no durable/business authority (ADR-0018 "PostgreSQL and
//! storage": storage I/O and control transport are execution; durable
//! acceptance/sealing/verification are `bamepd`'s). Still out of scope here:
//! the Worker-owned HTTPS/TLS data-plane listener and routes, and the
//! orchestration that composes `ipc` + `storage` behind them (Phase E2).

pub mod config;
pub mod ipc;
pub mod storage;
pub mod tls;
