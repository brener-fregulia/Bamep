//! Issue #61 CP2 — disposable PostgreSQL database helper.
//!
//! Adapted from `crates/server/tests/support/mod.rs::TestDatabase` (the exact
//! isolation discipline in `docs/development/testing.md` "Test isolation"):
//! create a uniquely named database, migrate it through the real Adapter
//! `bamep_server::adapters::postgres::connect` entry point, drop it on
//! teardown. Every database this helper creates carries the
//! `bamep_issue61_cp2_` prefix it generates itself, so teardown never touches
//! a database it did not create.
//!
//! Admin connection: `BAMEP_ISSUE61_CP2_ADMIN_URL`, else `BAMEP_TEST_PG_ADMIN_URL`,
//! else a peer-authenticated DSN derived for the current OS user over the local
//! PostgreSQL Unix socket (no password, no `pg_hba.conf` change), targeting the
//! `postgres` maintenance database. The role must be able to CREATE/DROP
//! databases.

use sqlx::PgPool;
use uuid::Uuid;

fn admin_url() -> String {
    for key in ["BAMEP_ISSUE61_CP2_ADMIN_URL", "BAMEP_TEST_PG_ADMIN_URL"] {
        if let Some(url) = std::env::var(key).ok().filter(|u| !u.is_empty()) {
            return url;
        }
    }
    let nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let user = nonempty("USER")
        .or_else(|| nonempty("LOGNAME"))
        .unwrap_or_else(|| {
            panic!(
                "cannot derive a PostgreSQL admin connection: neither $USER nor $LOGNAME is set.\n\
                 Set BAMEP_ISSUE61_CP2_ADMIN_URL to a DSN whose role can CREATE/DROP databases, e.g.\n  \
                 BAMEP_ISSUE61_CP2_ADMIN_URL=postgresql://<role>@%2Frun%2Fpostgresql/postgres"
            )
        });
    let socket = local_pg_socket_dir().replace('/', "%2F");
    match nonempty("PGPORT") {
        Some(port) => format!("postgresql://{user}@{socket}:{port}/postgres"),
        None => format!("postgresql://{user}@{socket}/postgres"),
    }
}

fn local_pg_socket_dir() -> String {
    if let Ok(pghost) = std::env::var("PGHOST") {
        if pghost.starts_with('/') {
            return pghost;
        }
    }
    for candidate in ["/run/postgresql", "/var/run/postgresql"] {
        if std::path::Path::new(candidate).exists() {
            return candidate.to_string();
        }
    }
    "/tmp".to_string()
}

/// scheme://<redacted>@host/db — never surfaces userinfo or query string.
pub fn redact_dsn(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("postgresql", url));
    let rest = rest.split('?').next().unwrap_or(rest);
    let (authority, db) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{scheme}://<redacted>@{host}/{db}")
}

fn with_database(url: &str, db_name: &str) -> String {
    let (prefix, _existing_db) = url
        .rsplit_once('/')
        .expect("admin URL must include a database path segment");
    format!("{prefix}/{db_name}")
}

pub struct TestDatabase {
    pub pool: PgPool,
    #[allow(dead_code)]
    pub db_url: String,
    name: String,
    admin_url: String,
}

impl TestDatabase {
    /// Creates a uniquely named `bamep_issue61_cp2_`-prefixed database and
    /// migrates it via the real Adapter connect path.
    pub async fn setup() -> Self {
        let admin_url = admin_url();
        let name = format!("bamep_issue61_cp2_{}", Uuid::new_v4().simple());

        let admin_pool = PgPool::connect(&admin_url).await.unwrap_or_else(|e| {
            panic!(
                "cannot connect to the admin PostgreSQL database `{}`: {e}\n\
                 By default this harness connects as the current OS user over the local \
                 PostgreSQL Unix socket (peer authentication).",
                redact_dsn(&admin_url)
            )
        });
        sqlx::query(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
            .execute(&admin_pool)
            .await
            .unwrap_or_else(|e| panic!("failed to CREATE the disposable database `{name}`: {e}"));
        admin_pool.close().await;

        let db_url = with_database(&admin_url, &name);
        let pool = bamep_server::adapters::postgres::connect(&db_url)
            .await
            .expect("connect to and migrate the fresh disposable database");

        Self {
            pool,
            db_url,
            name,
            admin_url,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Closes the pool and drops the database (FORCE). Must be called
    /// explicitly (no async Drop). A panic before this leaves only a
    /// trivially identifiable `bamep_issue61_cp2_*` database behind.
    pub async fn teardown(self) {
        self.pool.close().await;
        let admin_pool = PgPool::connect(&self.admin_url).await.unwrap_or_else(|e| {
            panic!(
                "cannot reconnect to the admin PostgreSQL database `{}` for teardown: {e} \
                 (the disposable database `{}` may need a manual DROP)",
                redact_dsn(&self.admin_url),
                self.name
            )
        });
        sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"DROP DATABASE IF EXISTS "{}" WITH (FORCE)"#,
            self.name
        )))
        .execute(&admin_pool)
        .await
        .expect("drop disposable database");
        admin_pool.close().await;
    }
}
