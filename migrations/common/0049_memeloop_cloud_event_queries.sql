CREATE INDEX IF NOT EXISTS memeloop_cloud_events_key_time_idx
    ON memeloop_cloud_subscription_events
       (key_id, created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS memeloop_cloud_events_tenant_time_idx
    ON memeloop_cloud_subscription_events
       (tenant_id, created_at DESC, id DESC);
