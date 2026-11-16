-- Existing failures have no retained capacity identity. Do not manufacture
-- account-wide permission from a historical model-specific rejection.
ALTER TABLE upstream_supplier_availability
    ADD COLUMN capacity_scope TEXT NOT NULL DEFAULT 'unknown';
