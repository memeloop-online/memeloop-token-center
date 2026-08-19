CREATE INDEX service_principals_created_cursor_idx
    ON service_principals (created_at DESC, id DESC);

CREATE INDEX upstream_accounts_created_cursor_idx
    ON upstream_accounts (created_at DESC, id DESC);

CREATE INDEX upstream_accounts_tenant_created_cursor_idx
    ON upstream_accounts (tenant_id, created_at DESC, id DESC);

CREATE INDEX model_routes_created_cursor_idx
    ON model_routes (created_at DESC, id DESC);

CREATE INDEX model_routes_tenant_created_cursor_idx
    ON model_routes (tenant_id, created_at DESC, id DESC);

-- The upstream page reports route_count for at most 100 providers. Without
-- this lookup index every row performs a full model_routes scan.
CREATE INDEX model_routes_upstream_account_idx
    ON model_routes (upstream_account_id);
