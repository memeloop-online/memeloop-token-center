ALTER TABLE usage_reservations
    ADD COLUMN enforcement_mode TEXT NOT NULL DEFAULT 'prepaid'
        CHECK (enforcement_mode IN ('prepaid', 'metered_unlimited'));

CREATE TABLE IF NOT EXISTS metered_usage_projection_outbox (
    reservation_id TEXT PRIMARY KEY REFERENCES usage_reservations(id) ON DELETE CASCADE,
    account_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    actual_micros BIGINT NOT NULL CHECK (actual_micros >= 0),
    created_at BIGINT NOT NULL,
    projected_at BIGINT
);

CREATE INDEX IF NOT EXISTS metered_usage_projection_pending_idx
    ON metered_usage_projection_outbox (projected_at, created_at, reservation_id);
