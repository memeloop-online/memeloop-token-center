-- Stable descending keyset pagination for MemeLoop Web reconciliation. The
-- account ledger index already exists from v7/v12, while credential listing needs
-- matching global, tenant, and principal access paths.
CREATE INDEX IF NOT EXISTS key_records_created_cursor_idx
    ON key_records (created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS key_records_tenant_created_cursor_idx
    ON key_records (tenant_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS key_records_principal_created_cursor_idx
    ON key_records (principal_id, created_at DESC, id DESC);
