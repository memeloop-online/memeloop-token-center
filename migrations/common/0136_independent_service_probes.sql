CREATE TABLE upstream_service_probes (
    upstream_account_id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    error_code TEXT,
    latency_ms BIGINT,
    checked_at BIGINT NOT NULL,
    FOREIGN KEY (upstream_account_id) REFERENCES upstream_accounts(id) ON DELETE CASCADE
);

ALTER TABLE transport_proxy_groups ADD COLUMN last_probe_status TEXT;
ALTER TABLE transport_proxy_groups ADD COLUMN last_probe_checked_at BIGINT;
