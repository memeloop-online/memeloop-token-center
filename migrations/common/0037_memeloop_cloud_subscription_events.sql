CREATE TABLE IF NOT EXISTS memeloop_cloud_subscription_events (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    entitlement_id TEXT NOT NULL,
    event_key_hash TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    version BIGINT NOT NULL,
    subscription_status TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    UNIQUE(tenant_id, event_key_hash)
);

CREATE INDEX IF NOT EXISTS memeloop_cloud_events_principal_time_idx
    ON memeloop_cloud_subscription_events
       (tenant_id, principal_id, created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS memeloop_cloud_events_entitlement_version_idx
    ON memeloop_cloud_subscription_events
       (entitlement_id, version DESC, created_at DESC);
