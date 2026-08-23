# Persistence

This document defines current Bamep Server persistence implementation and schema-evolution
conventions. Backend rationale belongs to ADR-0013; normative durability/event semantics belong
to `docs/specifications/m0-persistence-observability-and-domain-events.md`.

## Current implementation

- PostgreSQL is the selected backend (ADR-0013).
- SQLx is the current Rust PostgreSQL toolkit.
- Persistence-specific APIs and types stay inside `crates/server/src/adapters/postgres/`.
- Domain and Application must not depend on `sqlx`, `PgPool`, PostgreSQL row types, or
  persistence annotations.
- Do not add another backend, persistence framework, or portability abstraction without a
  concrete approved requirement.

Current repository structure and code are final evidence for implementation details.

## SQL and model mapping

Bamep currently uses explicit SQL rather than an ORM.

```text
database row/model != Domain entity
```

Adapter-local row/DTO types may map between SQL results and Domain values. Domain types must not
gain SQLx/ORM derives merely for persistence convenience.

Queryable lifecycle, correlation, scheduling, reconciliation, audit, and safety state should use
relational columns, constraints, and indexes. `JSONB` is for genuinely variable/opaque payloads,
not whole-aggregate serialization. ADR-0013 owns the relational-first rationale.

### Closed categorical values

For durable closed low-cardinality vocabularies:

- prefer PostgreSQL `ENUM` when the vocabulary is intentionally closed;
- use `TEXT` for open-ended/descriptive values;
- use numeric codes only when a demonstrated storage/performance need justifies the readability
  cost;
- PostgreSQL enum representations remain Adapter-local and map explicitly to/from Domain types;
- enum-label changes follow the migration-history rules below.

## Query style

The current baseline uses runtime-checked SQL such as:

- `sqlx::query`;
- `sqlx::query_scalar`;
- parameter binding and explicit `Row` mapping.

`query!` / `query_as!` compile-time macros are not required as a project baseline. Compilation
must not require a live database or generated `.sqlx` metadata solely for query checking.

The SQLx `macros` feature remains required while the Adapter uses `sqlx::migrate!`.

## Migrations

Schema changes use versioned SQL migrations under:

`crates/server/migrations/`

Conventions:

1. use readable monotonic names such as `0001_initial_schema.sql`;
2. do not hide schema evolution in startup-time Rust strings or ad-hoc `ALTER` logic;
3. migrations are transactional by default when PostgreSQL supports the operation;
4. non-transactional migrations require explicit justification and validation;
5. Server startup applies pending embedded migrations before becoming operational;
6. migration failure must fail startup rather than run against a partially compatible schema;
7. runtime must not require the external SQLx CLI or an on-disk migrations directory;
8. use database constraints (`NOT NULL`, `UNIQUE`, foreign keys, `CHECK`, indexes) where
   appropriate to protect durable invariants;
9. destructive/compatibility-sensitive changes require explicit upgrade, backup, and recovery
   consideration.

Do not invent a production backup/version-retention policy that the project has not specified.

## Migration-history phases

### Pre-baseline development — current phase

Until Bamep has either:

1. a persistent non-disposable pilot database intended for in-place upgrade; or
2. a released Server version establishing a supported persistent schema,

development/test databases are disposable and the initial migration history may be rebased.

During this phase:

- existing development migrations may be edited, renamed, reordered, consolidated, or removed;
- schema corrections should normally be folded into the baseline instead of creating artificial
  forward migrations;
- do not add backfills, dual-schema compatibility, or transitional behavior solely to preserve
  disposable development data;
- after baseline changes, recreate stale local development databases;
- validate that the complete schema can be created in a fresh PostgreSQL database;
- prefer a small clean baseline, keeping the still-initial schema in `0001_initial_schema.sql`
  when appropriate.

Git history preserves pre-baseline implementation evolution; migration history does not need to.

### Frozen migration history

At the first condition above, any migration that may have reached a supported persistent
database becomes immutable and migration history becomes append-only.

After freeze:

- every schema change gets a new monotonic migration;
- applied/released migrations must not be edited, renamed, deleted, reordered, or squashed;
- checksum-changing corrections use a new forward migration;
- supported prior schema/release states become explicit upgrade-test inputs.

## Migration validation

Follow `docs/development/testing.md`.

Persistence/migration validation must use real PostgreSQL. At minimum:

- migrations apply cleanly to a fresh isolated disposable database;
- SQLite/in-memory behavior is not evidence of PostgreSQL behavior;
- after freeze, supported prior schema states are also tested through the forward upgrade path;
- never claim an upgrade path was tested unless it actually ran.

## SQL line endings

Migration SQL files use LF to keep embedded migration checksums stable across Windows/Linux
development.

`.gitattributes` owns the repository rule:

```text
crates/server/migrations/*.sql text eol=lf
```

Do not broaden unrelated line-ending policy while changing persistence migrations.

## Related

- ADR-0013 — PostgreSQL backend decision and relational-first rationale.
- `docs/specifications/m0-persistence-observability-and-domain-events.md` — normative persistence,
  event, audit, correlation, and crash-ordering contract.
- `docs/architecture/README.md` — current implemented boundaries.
- `docs/development/testing.md` — persistence/migration validation policy.
