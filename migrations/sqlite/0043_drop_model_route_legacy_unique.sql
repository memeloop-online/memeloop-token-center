-- SQLite cannot drop a table-level UNIQUE constraint in place.  Rebuild the
-- small control-plane table without changing stable IDs or compatibility
-- columns. High-volume request history is not touched.
ALTER TABLE model_routes RENAME TO model_routes_legacy_0043;

CREATE TABLE model_routes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    public_model TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    upstream_model TEXT NOT NULL,
    protocol TEXT NOT NULL,
    priority BIGINT NOT NULL,
    enabled BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

INSERT INTO model_routes (
    id,
    tenant_id,
    public_model,
    upstream_account_id,
    upstream_model,
    protocol,
    priority,
    enabled,
    created_at,
    updated_at
)
SELECT
    id,
    tenant_id,
    public_model,
    upstream_account_id,
    upstream_model,
    protocol,
    priority,
    enabled,
    created_at,
    updated_at
FROM model_routes_legacy_0043;

DROP TABLE model_routes_legacy_0043;

CREATE INDEX model_routes_lookup_idx
    ON model_routes (tenant_id, public_model, protocol, enabled, priority);
CREATE INDEX model_routes_created_cursor_idx
    ON model_routes (created_at DESC, id DESC);
CREATE INDEX model_routes_tenant_created_cursor_idx
    ON model_routes (tenant_id, created_at DESC, id DESC);
CREATE INDEX model_routes_upstream_account_idx
    ON model_routes (upstream_account_id);
