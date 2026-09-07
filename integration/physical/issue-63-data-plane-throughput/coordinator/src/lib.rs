//! Issue #63 Stage 1 coordinator — library surface (the pure state machine),
//! kept separate from `main.rs` so `cargo test` exercises it without any I/O.

pub mod matrix;
pub mod state;
