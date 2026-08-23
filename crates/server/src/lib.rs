//! Bamep Server library crate.
//!
//! Internal module boundaries (`m0-stack-and-boundaries-baseline.md`
//! "Component responsibilities and boundaries") are responsibility/
//! dependency boundaries preserved within this one crate, not a mandate for
//! separate crates per boundary: `application` depends on `ports` and
//! `bamep_domain` only; `adapters` implements `ports` and is the only place
//! allowed to depend on `sqlx` (ADR-0013 "PostgreSQL persistence backend
//! baseline").

pub mod adapters;
pub mod application;
pub mod ports;
pub mod runtime;
