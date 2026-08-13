CREATE TABLE IF NOT EXISTS subscription_entitlements (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    external_subscription_id TEXT NOT NULL,
    status TEXT NOT NULL,
    version BIGINT NOT NULL,
    current_cycle_id TEXT,
    replaced_by_entitlement_id TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE(tenant_id, provider, external_subscription_id)
);

CREATE TABLE IF NOT EXISTS entitlement_cycles (
    id TEXT PRIMARY KEY,
    entitlement_id TEXT NOT NULL,
    external_cycle_id TEXT NOT NULL,
    period_start BIGINT NOT NULL,
    period_end BIGINT NOT NULL,
    currency TEXT NOT NULL,
    desired_micros BIGINT NOT NULL,
    funded_micros BIGINT NOT NULL,
    consumed_micros BIGINT NOT NULL,
    status TEXT NOT NULL,
    proration_json TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE(entitlement_id, external_cycle_id)
);

CREATE TABLE IF NOT EXISTS entitlement_reconciliations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    response_json TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE(tenant_id, idempotency_key)
);

CREATE TABLE IF NOT EXISTS entitlement_usage_allocations (
    id TEXT PRIMARY KEY,
    entitlement_cycle_id TEXT NOT NULL,
    usage_ledger_entry_id TEXT NOT NULL,
    amount_micros BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE(entitlement_cycle_id, usage_ledger_entry_id)
);

ALTER TABLE ledger_entries ADD COLUMN entitlement_cycle_id TEXT;

CREATE INDEX IF NOT EXISTS subscription_entitlements_account_idx
    ON subscription_entitlements (account_id, updated_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS subscription_entitlements_tenant_idx
    ON subscription_entitlements (tenant_id, provider, external_subscription_id);

CREATE INDEX IF NOT EXISTS entitlement_cycles_active_accounting_idx
    ON entitlement_cycles (entitlement_id, status, period_end, id);

CREATE INDEX IF NOT EXISTS entitlement_reconciliations_tenant_time_idx
    ON entitlement_reconciliations (tenant_id, created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS entitlement_usage_allocations_ledger_idx
    ON entitlement_usage_allocations (usage_ledger_entry_id);
