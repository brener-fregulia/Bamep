//! Issue #63 Stage 2 — pure off-device matrix engine. THROWAWAY Spike.
//!
//! This crate is the DETERMINISTIC AUTHORITY the Stage-2 coordinator, the
//! Issue-63 one-transfer harness and the WinPE transfer probe compose against.
//! It contains NO I/O of any kind: every function is a pure transform over
//! explicit inputs, so `cargo test` proves it on the Linux dev host with no
//! network / database / filesystem / device / Windows.
//!
//! Modules:
//!
//! - [`matrix`] — the exact 36-case physical matrix plan + chunk arithmetic +
//!   the pre-transfer chunk-size agreement gate;
//! - [`safety`] — the physical source-safety predicate (fail-closed) and the
//!   instrumented ordering gate that guarantees a rejection performs ZERO bulk
//!   source reads;
//! - [`budget`] — the Stage-3 disk-budget preflight (72 GiB payload, default
//!   90 GiB gate), pure;
//! - [`lifecycle`] — the typed per-case state machine + the matrix sequencer
//!   that stops handing out cases after the first FAILED / CONTAMINATED case;
//! - [`result`] — the per-case NDJSON result record schema;
//! - [`analysis`] — measured-only aggregation + within-cycle paired ratios (the
//!   Phase-A statistical helpers, re-derived here because the Phase-A benchmark
//!   is a binary with no library target and is frozen evidence that must not be
//!   modified).
//!
//! NOTHING here arms a physical transfer. There is no `main`, no runner, no
//! socket. PHYSICAL MATRIX NOT ARMED.

pub mod analysis;
pub mod budget;
pub mod lifecycle;
pub mod matrix;
pub mod result;
pub mod safety;
/// Issue #63 **Stage 4** micro-matrix (64 MiB serial vs prep-ahead depth-2) +
/// paired analysis + Worker PUT decomposition. Separate from the Stage-3
/// [`matrix`]/[`result`]/[`analysis`] authority; nothing here arms a transfer.
pub mod stage4;

/// One mebibyte.
pub const MIB: u64 = 1024 * 1024;
/// One gibibyte.
pub const GIB: u64 = 1024 * MIB;
