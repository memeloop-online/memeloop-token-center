-- One immutable canonical delivery per durable revision. No body duplication:
-- snapshots carry the existing archive/object references in the read model.
-- This queue is deleted only after exact ClickHouse delivery acknowledgement.
CREATE TABLE request_history_immutable_outbox (
    revision BIGINT PRIMARY KEY,
    source TEXT NOT NULL,
    request_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    snapshot TEXT NOT NULL,
    delivery_snapshot TEXT
);
CREATE INDEX request_history_immutable_outbox_identity
    ON request_history_immutable_outbox(tenant_id, source, request_id, revision);
