//! Worker TLS identity loading (Issue #37 "TLS identity provisioning"): the
//! host-local protected-file mechanism selected for #37 — certificate/key
//! paths supplied through process configuration, never key bytes over UDS
//! IPC (ADR-0018 "TLS identity": "Key material must not be transported
//! through the normal Server<->Worker application IPC protocol").
//!
//! #37 proves the loaded identity is rustls-usable and matches the exact
//! Server leaf certificate; it does not bind any production HTTPS
//! listener — that is #39's responsibility.

pub mod identity;

pub use identity::{
    build_server_config, load_server_identity, ServerTlsIdentity, TlsIdentityError,
};
