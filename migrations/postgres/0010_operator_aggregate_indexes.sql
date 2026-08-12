CREATE INDEX IF NOT EXISTS key_records_tenant_id_idx
    ON key_records (tenant_id, id);
