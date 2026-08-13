DROP INDEX IF EXISTS key_records_provisioning_idempotency_idx;
CREATE UNIQUE INDEX key_records_provisioning_idempotency_idx
    ON key_records (tenant_id, provisioning_idempotency_key)
    WHERE provisioning_idempotency_key IS NOT NULL;

ALTER TABLE ledger_entries
    DROP CONSTRAINT IF EXISTS ledger_entries_idempotency_key_key;
CREATE UNIQUE INDEX IF NOT EXISTS ledger_entries_account_idempotency_idx
    ON ledger_entries (account_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
