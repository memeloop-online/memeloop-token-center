-- Metered settlement writes are durable before their low-contention
-- observability projections.  A lease lets several worker processes drain
-- that outbox concurrently while an abandoned claim becomes retryable.
ALTER TABLE metered_usage_projection_outbox
    ADD COLUMN lease_owner TEXT;

ALTER TABLE metered_usage_projection_outbox
    ADD COLUMN lease_expires_at BIGINT;

ALTER TABLE metered_usage_projection_outbox
    ADD COLUMN attempts BIGINT NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS metered_usage_projection_claim_idx
    ON metered_usage_projection_outbox (
        projected_at,
        lease_expires_at,
        created_at,
        reservation_id
    );
