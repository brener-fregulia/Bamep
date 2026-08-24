-- Initial relational-first Server schema for the Endpoint identity,
-- credential, current-boot, domain-event, and audit responsibilities currently
-- implemented by WP1. Queryable lifecycle and security state uses native
-- PostgreSQL types and columns; JSONB is reserved for event-specific payloads.

CREATE TYPE endpoint_identity_state AS ENUM (
    'PendingEnrollment',
    'Enrolled',
    'Retired'
);

CREATE TYPE domain_event_type AS ENUM (
    'EndpointPendingEnrollment',
    'EndpointEnrolled',
    'OperatorDecisionRecorded',
    'InventoryRevisionRecorded',
    'JobStarted',
    'JobSucceeded',
    'JobFailed',
    'JobStepFailed',
    'JobCancelled'
);

CREATE TYPE audit_actor_kind AS ENUM (
    'operator',
    'system'
);

CREATE TYPE credential_slot_role AS ENUM (
    'predecessor',
    'successor'
);

CREATE TYPE trusted_bootstrap_state AS ENUM (
    'NotEstablished',
    'Established'
);

-- Durable Endpoint hardware-confidence dimension
-- (m0-endpoint-identity-lifecycle.md "3. Hardware confidence"). No column
-- DEFAULT is declared: the Domain/Application layer (not PostgreSQL) owns the
-- `Consistent` initial-baseline semantic, and every insert supplies it
-- explicitly.
CREATE TYPE hardware_confidence_state AS ENUM (
    'Consistent',
    'LoweredConfidence',
    'Conflict'
);

-- Durable Endpoint identity and authoritative current-boot projection.
-- Inventory is correlation evidence, never authentication or permanent
-- identity. A NULL current-boot triple represents unknown historical state and
-- fails closed; otherwise all three values must be present. The projection is
-- intentionally independent of BootContext row locks: its pairing with the
-- issuance record is established by the atomic first-contact/genuine-reboot
-- transaction, not by a foreign key, trigger, or runtime cross-table lookup.
CREATE TABLE endpoints (
    id UUID PRIMARY KEY,
    inventory_signal TEXT NOT NULL UNIQUE,
    identity_state endpoint_identity_state NOT NULL,
    hardware_confidence hardware_confidence_state NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    current_boot_context_id BYTEA
        CHECK (current_boot_context_id IS NULL OR octet_length(current_boot_context_id) = 16),
    current_boot_nonce BYTEA
        CHECK (current_boot_nonce IS NULL OR octet_length(current_boot_nonce) = 32),
    trusted_bootstrap_state trusted_bootstrap_state,
    current_inventory_revision_id UUID,
    CONSTRAINT endpoints_current_boot_all_or_none CHECK (
        (current_boot_context_id IS NULL AND current_boot_nonce IS NULL AND trusted_bootstrap_state IS NULL)
        OR (current_boot_context_id IS NOT NULL AND current_boot_nonce IS NOT NULL AND trusted_bootstrap_state IS NOT NULL)
    )
);

-- Opaque observed inventory is retained as JSONB while revision identity,
-- ownership, ordering time, and the Endpoint current pointer remain
-- relational and queryable.
CREATE TABLE inventory_revisions (
    endpoint_id UUID NOT NULL REFERENCES endpoints (id),
    revision_id UUID NOT NULL,
    inventory JSONB NOT NULL CHECK (jsonb_typeof(inventory) = 'object'),
    recorded_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (endpoint_id, revision_id)
);

ALTER TABLE endpoints ADD CONSTRAINT endpoints_current_inventory_revision_fk
    FOREIGN KEY (id, current_inventory_revision_id)
    REFERENCES inventory_revisions (endpoint_id, revision_id);

-- ADR-0012 bounded runtime-credential chain: one predecessor plus at most one
-- current unconfirmed successor. Verifiers are one-way opaque byte values;
-- queryable lifecycle and timestamp fields remain relational columns.
CREATE TABLE endpoint_credentials (
    endpoint_id UUID PRIMARY KEY REFERENCES endpoints (id),
    predecessor_verifier BYTEA NOT NULL,
    predecessor_issued_at TIMESTAMPTZ NOT NULL,
    predecessor_expires_at TIMESTAMPTZ NOT NULL,
    successor_verifier BYTEA,
    successor_issued_at TIMESTAMPTZ,
    successor_expires_at TIMESTAMPTZ,
    revoked BOOLEAN NOT NULL DEFAULT FALSE,
    CHECK (
        (successor_verifier IS NULL AND successor_issued_at IS NULL AND successor_expires_at IS NULL)
        OR (successor_verifier IS NOT NULL AND successor_issued_at IS NOT NULL AND successor_expires_at IS NOT NULL)
    )
);

-- Domain-event envelope fields remain queryable columns; payload contains only
-- the event-type-specific remainder. job_id is nullable because only
-- Job-scoped events (currently JobStarted/JobSucceeded/JobFailed/
-- JobStepFailed, Issues #32/#26) carry it; every other event type currently
-- implemented is Endpoint-scoped only. job_step_id is nullable further still
-- — only JobStepFailed (Issue #26) carries it. Their FKs are added below,
-- once the `jobs`/`job_steps` tables this table is declared ahead of exist —
-- mirroring `endpoints_current_inventory_revision_fk` above.
CREATE TABLE domain_events (
    event_id UUID PRIMARY KEY,
    event_type domain_event_type NOT NULL,
    event_version INTEGER NOT NULL DEFAULT 1,
    endpoint_id UUID NOT NULL REFERENCES endpoints (id),
    job_id UUID,
    job_step_id UUID,
    occurred_at TIMESTAMPTZ NOT NULL,
    payload JSONB NOT NULL
);

CREATE INDEX idx_domain_events_endpoint_id ON domain_events (endpoint_id);
CREATE INDEX idx_domain_events_job_id ON domain_events (job_id);

-- Durable safety-relevant audit records. Actor attribution is relational, and
-- the actor label must be present exactly for operator actors.
--
-- job_id/job_step_id/attempt_id/action_id are narrow optional correlation
-- columns (Issue #25): the destructive-dispatch commitment audit record
-- populates all four so its correlation is structural, never hidden solely in
-- `detail`. Every audit record predating #25 (e.g. enrollment approval)
-- leaves all four NULL. Their FK constraints are added below, once `jobs`,
-- `job_steps`, and `attempts` exist — mirroring `domain_events_job_id_fk`.
CREATE TABLE audit_records (
    audit_id UUID PRIMARY KEY,
    endpoint_id UUID NOT NULL REFERENCES endpoints (id),
    actor_kind audit_actor_kind NOT NULL,
    actor_label TEXT,
    occurred_at TIMESTAMPTZ NOT NULL,
    detail TEXT NOT NULL,
    job_id UUID,
    job_step_id UUID,
    attempt_id UUID,
    action_id UUID,
    CONSTRAINT audit_records_actor_label_check CHECK (
        (actor_kind = 'operator' AND actor_label IS NOT NULL)
        OR (actor_kind = 'system' AND actor_label IS NULL)
    )
);

CREATE INDEX idx_audit_records_endpoint_id ON audit_records (endpoint_id);

-- Durable BootContext issuance/redemption record. Identifiers and credential
-- verifiers are fixed-size byte representations, and inventory remains
-- correlation evidence only. boot_nonce is nullable solely so unknown
-- historical/pre-baseline rows can remain fail-closed; every new-flow context
-- supplies an exact 32-byte nonce. Multiple contexts may share an inventory
-- signal across distinct boot attempts.
CREATE TABLE boot_contexts (
    boot_context_id BYTEA PRIMARY KEY
        CHECK (octet_length(boot_context_id) = 16),
    verifier BYTEA NOT NULL
        CHECK (octet_length(verifier) = 48),
    issued_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
        CHECK (expires_at > issued_at),
    inventory_signal TEXT NOT NULL,
    resolved_endpoint_id UUID REFERENCES endpoints (id),
    boot_nonce BYTEA
        CHECK (boot_nonce IS NULL OR octet_length(boot_nonce) = 32)
);

-- Globally unique self-locating credential lookup projection. The composite
-- primary key permits one mapping per Endpoint/slot, while global lookup_id
-- uniqueness prevents ambiguous routing across all Endpoints and both roles.
CREATE TABLE endpoint_credential_lookups (
    endpoint_id UUID NOT NULL REFERENCES endpoints (id),
    slot credential_slot_role NOT NULL,
    lookup_id BYTEA NOT NULL
        CHECK (octet_length(lookup_id) = 16),
    PRIMARY KEY (endpoint_id, slot),
    UNIQUE (lookup_id)
);

CREATE TYPE job_state AS ENUM (
    'Pending',
    'Running',
    'Cancelling',
    'Succeeded',
    'Failed',
    'Cancelled'
);

CREATE TYPE job_step_state AS ENUM (
    'Pending',
    'PreconditionsSatisfied',
    'Dispatching',
    'Succeeded',
    'Failed',
    'Cancelled'
);

-- Durable Job: one workflow targeting one Endpoint
-- (m0-job-lifecycle-and-scheduling.md "Domain model"). Issue #24 durably
-- creates only rows in `Pending`; later scheduling/dispatch Work Packages own
-- the remaining lifecycle transitions and whatever additional durable
-- state/events/audit they require.
CREATE TABLE jobs (
    id UUID PRIMARY KEY,
    endpoint_id UUID NOT NULL REFERENCES endpoints (id),
    state job_state NOT NULL
);

CREATE INDEX idx_jobs_endpoint_id ON jobs (endpoint_id);

ALTER TABLE domain_events ADD CONSTRAINT domain_events_job_id_fk
    FOREIGN KEY (job_id) REFERENCES jobs (id);

-- Job-scoped Endpoint exclusivity (m0-job-lifecycle-and-scheduling.md
-- "Resource leases": "at most one Running/Cancelling Job owns one Endpoint at
-- a time"; Issue #32). The minimum durable representation already implied by
-- Job state: a partial unique index over the active states, rather than a
-- separate lease table. PostgreSQL itself enforces this atomically and
-- race-safely — a concurrent admission attempt against the losing Job's row
-- either waits behind the winner's uncommitted index entry and then fails
-- with a unique-violation once the winner commits, or succeeds outright if
-- the winner's transaction rolled back instead.
CREATE UNIQUE INDEX jobs_active_endpoint_exclusivity
    ON jobs (endpoint_id)
    WHERE state IN ('Running', 'Cancelling');

-- Durable JobStep: one ordered linear stage of its owning Job. step_order is
-- the explicit stable linear position the accepted linear-workflow baseline
-- requires; the UNIQUE constraint protects that ordering durably.
-- Branching/parallel/skip ordering is out of scope
-- (m0-job-lifecycle-and-scheduling.md "Out of scope").
--
-- authorized_inventory_revision_id/authorized_target_fingerprint are the
-- durable destructive-operation authorization snapshot (Issue #31): the
-- Server-owned inventory revision and opaque target-disk fingerprint
-- authorized for this JobStep at the moment Issue #31's Application path
-- (DestructiveIntentService) attaches them. Presence classifies this JobStep
-- as destructive for the M1 workflow path. Both columns are nullable and
-- CHECK'd all-or-none so PostgreSQL itself cannot persist a half-intent; they
-- are never rewritten once set (single-assignment for this M1 boundary — a
-- later explicit lifecycle/authorization operation would be required to
-- replace one). A later scheduling/dispatch Work Package consumes/revalidates
-- this snapshot; it does not attach it.
-- The Domain-owned minimum durable JobStep failure-reason vocabulary
-- (m0-job-lifecycle-and-scheduling.md "JobStep lifecycle"; Issue #26).
-- 'ReconciliationIndeterminate' belongs to #28 and is intentionally not
-- represented here yet.
CREATE TYPE job_step_failure_reason AS ENUM (
    'DispatchRejected',
    'ExecutionFailed'
);

CREATE TABLE job_steps (
    id UUID PRIMARY KEY,
    job_id UUID NOT NULL REFERENCES jobs (id),
    step_order INTEGER NOT NULL CHECK (step_order >= 0),
    state job_step_state NOT NULL,
    authorized_inventory_revision_id UUID,
    authorized_target_fingerprint TEXT,
    -- Set exactly once a JobStep reaches Failed through normal Agent action
    -- evidence (Issue #26); NULL at every other state.
    failure_reason job_step_failure_reason,
    UNIQUE (job_id, step_order),
    CONSTRAINT job_steps_destructive_intent_all_or_none CHECK (
        (authorized_inventory_revision_id IS NULL AND authorized_target_fingerprint IS NULL)
        OR (authorized_inventory_revision_id IS NOT NULL AND authorized_target_fingerprint IS NOT NULL)
    )
);

CREATE INDEX idx_job_steps_job_id ON job_steps (job_id);

-- The full authoritative Attempt state vocabulary
-- (m0-job-lifecycle-and-scheduling.md "Attempt lifecycle"). Issue #25
-- persists only 'Dispatched'; the remaining values are represented so later
-- Work Packages do not need a schema change.
CREATE TYPE attempt_state AS ENUM (
    'Dispatched',
    'InProgress',
    'AwaitingReconciliation',
    'Succeeded',
    'Failed',
    'Cancelled',
    'Rejected',
    'Indeterminate'
);

-- Durable Attempt: one concrete execution of a JobStep
-- (m0-job-lifecycle-and-scheduling.md "Domain model"; Issue #25). job_step_id
-- is deliberately NOT UNIQUE: a JobStep may accumulate more than one Attempt
-- over its lifetime once retry policy exists (later Work Packages). action_id
-- is globally UNIQUE and distinct from `id` even though correlated 1:1
-- (m0-persistence-observability-and-domain-events.md "Correlation").
CREATE TABLE attempts (
    id UUID PRIMARY KEY,
    job_step_id UUID NOT NULL REFERENCES job_steps (id),
    action_id UUID NOT NULL UNIQUE,
    state attempt_state NOT NULL
);

CREATE INDEX idx_attempts_job_step_id ON attempts (job_step_id);

ALTER TABLE audit_records ADD CONSTRAINT audit_records_job_id_fk
    FOREIGN KEY (job_id) REFERENCES jobs (id);
ALTER TABLE audit_records ADD CONSTRAINT audit_records_job_step_id_fk
    FOREIGN KEY (job_step_id) REFERENCES job_steps (id);
ALTER TABLE audit_records ADD CONSTRAINT audit_records_attempt_id_fk
    FOREIGN KEY (attempt_id) REFERENCES attempts (id);

CREATE INDEX idx_audit_records_job_step_id ON audit_records (job_step_id);

ALTER TABLE domain_events ADD CONSTRAINT domain_events_job_step_id_fk
    FOREIGN KEY (job_step_id) REFERENCES job_steps (id);

CREATE INDEX idx_domain_events_job_step_id ON domain_events (job_step_id);
