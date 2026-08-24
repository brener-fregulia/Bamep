//! Runtime Services (`m0-stack-and-boundaries-baseline.md` "Component
//! responsibilities and boundaries" — Runtime Services): in-process runtime
//! state that is never PostgreSQL-durable, as distinct from `adapters`
//! (persistence and protocol transports) and `application` (Domain
//! orchestration).

pub mod outbound_sessions;
pub mod presence;
pub mod reservation_registry;
pub mod resource_arbiter;
