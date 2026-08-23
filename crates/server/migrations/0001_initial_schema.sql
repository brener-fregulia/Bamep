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
    'InventoryRevisionRecorded'
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
-- the event-type-specific remainder.
CREATE TABLE domain_events (
    event_id UUID PRIMARY KEY,
    event_type domain_event_type NOT NULL,
    event_version INTEGER NOT NULL DEFAULT 1,
    endpoint_id UUID NOT NULL REFERENCES endpoints (id),
    occurred_at TIMESTAMPTZ NOT NULL,
    payload JSONB NOT NULL
);

CREATE INDEX idx_domain_events_endpoint_id ON domain_events (endpoint_id);

-- Durable safety-relevant audit records. Actor attribution is relational, and
-- the actor label must be present exactly for operator actors.
CREATE TABLE audit_records (
    audit_id UUID PRIMARY KEY,
    endpoint_id UUID NOT NULL REFERENCES endpoints (id),
    actor_kind audit_actor_kind NOT NULL,
    actor_label TEXT,
    occurred_at TIMESTAMPTZ NOT NULL,
    detail TEXT NOT NULL,
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
