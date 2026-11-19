-- Existing failures have no retained capacity identity. Do not manufacture
-- account-wide permission from a historical model-specific rejection.
ALTER TABLE upstream_supplier_availability
    ADD COLUMN capacity_scope TEXT NOT NULL DEFAULT 'unknown';

ALTER TABLE upstream_quota_observations ADD COLUMN observed_at BIGINT NOT NULL DEFAULT 0;

ALTER TABLE upstream_quota_observations ADD COLUMN started_at BIGINT NOT NULL DEFAULT 0;
