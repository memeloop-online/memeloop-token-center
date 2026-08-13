DROP INDEX IF EXISTS key_records_provisioning_idempotency_idx;
CREATE UNIQUE INDEX key_records_provisioning_idempotency_idx
    ON key_records (tenant_id, provisioning_idempotency_key)
    WHERE provisioning_idempotency_key IS NOT NULL;

ALTER TABLE ledger_entries RENAME TO ledger_entries_global_idempotency;
CREATE TABLE ledger_entries (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    key_id TEXT,
    kind TEXT NOT NULL,
    amount_micros BIGINT NOT NULL,
    currency TEXT NOT NULL,
    source TEXT NOT NULL,
    idempotency_key TEXT,
    created_at BIGINT NOT NULL,
    reference_entry_id TEXT
);
INSERT INTO ledger_entries
    (id, account_id, key_id, kind, amount_micros, currency, source,
     idempotency_key, created_at, reference_entry_id)
SELECT id, account_id, key_id, kind, amount_micros, currency, source,
       idempotency_key, created_at, reference_entry_id
  FROM ledger_entries_global_idempotency;
DROP TABLE ledger_entries_global_idempotency;

CREATE UNIQUE INDEX ledger_entries_account_idempotency_idx
    ON ledger_entries (account_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
CREATE UNIQUE INDEX ledger_entries_grant_reversal_reference_idx
    ON ledger_entries (reference_entry_id)
    WHERE kind = 'grant_reversal' AND reference_entry_id IS NOT NULL;
CREATE INDEX ledger_entries_account_time_idx
    ON ledger_entries (account_id, created_at DESC, id DESC);
