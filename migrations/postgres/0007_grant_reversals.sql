ALTER TABLE ledger_entries
    ADD COLUMN IF NOT EXISTS reference_entry_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS ledger_entries_grant_reversal_reference_idx
    ON ledger_entries (reference_entry_id)
    WHERE kind = 'grant_reversal' AND reference_entry_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS ledger_entries_account_time_idx
    ON ledger_entries (account_id, created_at DESC, id DESC);
