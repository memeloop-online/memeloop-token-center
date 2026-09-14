-- Attributed settlement adjustments are desired-state records.  The baseline
-- retains the original entitlement-funded portion because allocation rows are
-- deliberately reduced as credit is returned.
CREATE TABLE settlement_adjustment_baselines (
    account_id TEXT NOT NULL,
    settlement_id TEXT NOT NULL,
    request_kind TEXT NOT NULL CHECK (request_kind IN ('text', 'generation')),
    request_id TEXT NOT NULL,
    currency TEXT NOT NULL,
    gross_micros BIGINT NOT NULL CHECK (gross_micros >= 0),
    original_entitlement_micros BIGINT NOT NULL CHECK (original_entitlement_micros >= 0),
    created_at BIGINT NOT NULL,
    PRIMARY KEY (account_id, settlement_id),
    UNIQUE (request_kind, request_id)
);

CREATE TABLE settlement_adjustment_states (
    account_id TEXT NOT NULL,
    settlement_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    desired_rebate_micros BIGINT NOT NULL CHECK (desired_rebate_micros >= 0),
    version BIGINT NOT NULL CHECK (version > 0),
    decision_digest TEXT NOT NULL,
    source TEXT NOT NULL,
    last_event_id TEXT NOT NULL,
    last_applied_delta_micros BIGINT NOT NULL CHECK (last_applied_delta_micros >= 0),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (account_id, settlement_id, namespace)
);

-- Events are immutable; state only points at the event that established its
-- current desired value.  A higher version with an unchanged desired value is
-- still represented by a zero-delta event.
CREATE TABLE settlement_adjustment_events (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    settlement_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    request_kind TEXT NOT NULL CHECK (request_kind IN ('text', 'generation')),
    request_id TEXT NOT NULL,
    currency TEXT NOT NULL,
    desired_rebate_micros BIGINT NOT NULL CHECK (desired_rebate_micros >= 0),
    applied_delta_micros BIGINT NOT NULL CHECK (applied_delta_micros >= 0),
    cumulative_rebate_micros BIGINT NOT NULL CHECK (cumulative_rebate_micros >= 0),
    version BIGINT NOT NULL CHECK (version > 0),
    decision_digest TEXT NOT NULL,
    source TEXT NOT NULL,
    ledger_entry_id TEXT,
    created_at BIGINT NOT NULL,
    UNIQUE (account_id, settlement_id, namespace, version)
);

-- Zero-delta reconciliations have no ledger row, so this endpoint retains its
-- own account-scoped key/hash binding.  Ledger rows use a derived internal
-- key and do not share this public endpoint idempotency namespace.
CREATE TABLE settlement_adjustment_idempotencies (
    account_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    event_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (account_id, idempotency_key)
);

-- This retains the exact consumed-credit attribution removed by an event.
-- `rollback_sequence` is the deterministic tail walk order, so the original
-- allocation changes can be audited and reconstructed after mutable
-- entitlement_usage_allocations rows have been reduced or deleted.
CREATE TABLE settlement_adjustment_entitlement_rollbacks (
    event_id TEXT NOT NULL,
    rollback_sequence BIGINT NOT NULL CHECK (rollback_sequence > 0),
    entitlement_cycle_id TEXT NOT NULL,
    amount_micros BIGINT NOT NULL CHECK (amount_micros > 0),
    created_at BIGINT NOT NULL,
    PRIMARY KEY (event_id, rollback_sequence),
    UNIQUE (event_id, entitlement_cycle_id)
);

-- A rollback can reduce durable funding when a cycle was already cancelled,
-- expired, or its desired funding is now below the revised consumption.  Keep
-- that compensating funding reduction immutable beside the allocation audit.
CREATE TABLE settlement_adjustment_entitlement_funding_reductions (
    event_id TEXT NOT NULL,
    entitlement_cycle_id TEXT NOT NULL,
    amount_micros BIGINT NOT NULL CHECK (amount_micros > 0),
    created_at BIGINT NOT NULL,
    PRIMARY KEY (event_id, entitlement_cycle_id)
);

CREATE INDEX settlement_adjustment_events_settlement_idx
    ON settlement_adjustment_events (account_id, settlement_id, created_at DESC, id DESC);
CREATE INDEX settlement_adjustment_allocations_usage_idx
    ON entitlement_usage_allocations (usage_ledger_entry_id, entitlement_cycle_id);
