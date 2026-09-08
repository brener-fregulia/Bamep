//! Issue #63 Stage 1 coordinator — library surface (the pure state machine),
//! kept separate from `main.rs` so `cargo test` exercises it without any I/O.

pub mod matrix;
pub mod matrix_net;
/// Issue #63 Stage 4 — ARMED networked wiring for the 64 MiB serial-vs-prep-ahead
/// micro-matrix. Reached only via `coordinator --stage4 --arm`.
pub mod stage4_net;
pub mod state;
