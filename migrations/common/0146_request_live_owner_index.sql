-- Seek live leases without scanning completed history or expired settlements.
-- On PostgreSQL, creating this index on the partitioned parent indexes every
-- existing partition; future PARTITION OF tables inherit it automatically.
CREATE INDEX request_records_live_owner_expiry_idx
    ON request_records (proxy_owner_expires_at DESC, id DESC)
    WHERE completed_at IS NULL AND proxy_owner_expires_at IS NOT NULL;
