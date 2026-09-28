-- Accepted text requests retain the exact route and billing inputs used at
-- admission. Historical projections must never recover them from mutable routes.
ALTER TABLE request_records ADD COLUMN upstream_model TEXT;
ALTER TABLE request_records ADD COLUMN price_snapshot_json TEXT;

