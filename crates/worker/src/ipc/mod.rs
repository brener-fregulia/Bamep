//! The Worker-side half of the `bamepd` <-> Worker UDS control boundary
//! (Issue #37; `docs/specifications/m1-worker-data-plane-control-contract.md`):
//! Worker is the reconnecting UDS client, `bamepd` is the UDS server
//! (ADR-0018 "Server<->Worker IPC").

pub mod authority;
pub mod authorization_client;
pub mod client;

pub use authority::{AuthorityPhase, AuthoritySnapshot, AuthorityTracker};
pub use authorization_client::{channel as authorization_channel, AuthorizationClient, QueryError};
pub use client::run_client_loop;
