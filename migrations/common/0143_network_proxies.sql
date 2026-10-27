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
    probe_successes BIGINT NOT NULL DEFAULT 0,
    probe_requested BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
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

CREATE TABLE network_proxy_selection_policies (
    account_id TEXT PRIMARY KEY REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    version BIGINT NOT NULL CHECK (version > 0),
    mode TEXT NOT NULL CHECK (mode IN ('manual', 'auto', 'plugin')),
    proxy_id TEXT REFERENCES network_proxies(id),
    group_id TEXT REFERENCES transport_proxy_groups(id),
    plugin_id TEXT,
    plugin_version TEXT,
    config_json TEXT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE INDEX network_proxy_selection_policies_proxy ON network_proxy_selection_policies(proxy_id);

CREATE TABLE network_proxy_attempt_signals (
    request_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    proxy_id TEXT NOT NULL REFERENCES network_proxies(id) ON DELETE CASCADE,
    proxy_version BIGINT NOT NULL,
    observed_at BIGINT NOT NULL,
    outcome TEXT NOT NULL DEFAULT 'pending',
    request_payload_bytes BIGINT,
    response_payload_bytes BIGINT,
    next_refresh_at BIGINT NOT NULL,
    PRIMARY KEY (request_id, attempt_id)
);
CREATE INDEX network_proxy_attempt_signals_window ON network_proxy_attempt_signals(proxy_id, proxy_version, observed_at);
CREATE INDEX network_proxy_attempt_signals_refresh ON network_proxy_attempt_signals(next_refresh_at);
