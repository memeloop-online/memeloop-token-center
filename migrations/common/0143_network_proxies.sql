CREATE TABLE network_proxies (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    version BIGINT NOT NULL CHECK (version > 0),
    proxy_ciphertext TEXT NOT NULL,
    probe_enabled BIGINT NOT NULL DEFAULT 1,
    probe_target TEXT NOT NULL DEFAULT 'https://openai.com/',
    probe_interval_ms BIGINT NOT NULL DEFAULT 60000,
    probe_attempts BIGINT NOT NULL DEFAULT 2,
    last_probe_status TEXT,
    last_probe_checked_at BIGINT,
    last_probe_latency_ms BIGINT,
    probe_requested BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (tenant_id, name)
);
CREATE TABLE network_proxy_group_members (
    group_id TEXT NOT NULL REFERENCES transport_proxy_groups(id) ON DELETE CASCADE,
    proxy_id TEXT NOT NULL REFERENCES network_proxies(id),
    position BIGINT NOT NULL,
    PRIMARY KEY (group_id, proxy_id),
    UNIQUE (group_id, position)
);
CREATE INDEX network_proxy_group_members_proxy ON network_proxy_group_members(proxy_id);
ALTER TABLE transport_proxy_groups ADD COLUMN members_normalized BIGINT NOT NULL DEFAULT 0;
