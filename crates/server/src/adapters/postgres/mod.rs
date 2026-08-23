//! PostgreSQL Adapter behind the `EndpointRepository` and
//! `BootContextRepository` Ports (ADR-0013 "PostgreSQL persistence backend
//! baseline"). Domain and Application code never reference `sqlx` or
//! PostgreSQL-specific types directly — only this module does.
//! Composition/exposure only: schema evolution lives in
//! `../../../migrations`, the `EndpointRepository` implementation lives in
//! `repository.rs`, and the `BootContextRepository` implementation lives in
//! `boot_context_repository.rs`.

mod boot_context_repository;
mod credential_redemption_repository;
mod inventory_repository;
mod job_repository;
mod repository;
mod shared;

pub use boot_context_repository::PostgresBootContextRepository;
pub use credential_redemption_repository::PostgresCredentialRedemptionRepository;
pub use inventory_repository::PostgresInventoryRepository;
pub use job_repository::PostgresJobRepository;
pub use repository::PostgresEndpointRepository;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Compiled-in migration set (`crates/server/migrations/`), embedded at
/// build time so the running Server never depends on an external migration
/// CLI or an on-disk migrations directory at runtime.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Connects a bounded pool to `database_url` and applies every pending
/// migration. The only entry point that should be used to obtain a
/// [`PostgresEndpointRepository`] outside of tests.
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}
