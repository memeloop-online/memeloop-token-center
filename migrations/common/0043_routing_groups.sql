-- Stable IDs remain globally unique, while the composite keys below make every
-- association tenant-safe at the database boundary.  A mismatched tenant can
-- therefore never create a cross-tenant routing edge, even if an application
-- caller supplies otherwise valid object IDs.
CREATE UNIQUE INDEX upstream_accounts_tenant_id_id_unique
    ON upstream_accounts (tenant_id, id);
CREATE UNIQUE INDEX model_routes_tenant_id_id_unique
    ON model_routes (tenant_id, id);
CREATE UNIQUE INDEX key_records_tenant_id_id_unique
    ON key_records (tenant_id, id);

CREATE TABLE provider_groups (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (tenant_id, normalized_name),
    UNIQUE (tenant_id, id),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    CHECK (LENGTH(TRIM(name)) BETWEEN 1 AND 128),
    CHECK (LENGTH(normalized_name) BETWEEN 1 AND 128),
    CHECK (created_at >= 0),
    CHECK (updated_at >= created_at)
);

CREATE TABLE upstream_account_provider_groups (
    tenant_id TEXT NOT NULL,
    provider_group_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (provider_group_id, upstream_account_id),
    FOREIGN KEY (tenant_id, provider_group_id)
        REFERENCES provider_groups(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, upstream_account_id)
        REFERENCES upstream_accounts(tenant_id, id) ON DELETE CASCADE,
    CHECK (created_at >= 0)
);
CREATE INDEX upstream_account_provider_groups_tenant_group_idx
    ON upstream_account_provider_groups
       (tenant_id, provider_group_id, upstream_account_id);
CREATE INDEX upstream_account_provider_groups_tenant_account_idx
    ON upstream_account_provider_groups
       (tenant_id, upstream_account_id, provider_group_id);

CREATE TABLE route_groups (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (tenant_id, normalized_name),
    UNIQUE (tenant_id, id),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    CHECK (LENGTH(TRIM(name)) BETWEEN 1 AND 128),
    CHECK (LENGTH(normalized_name) BETWEEN 1 AND 128),
    CHECK (created_at >= 0),
    CHECK (updated_at >= created_at)
);

CREATE TABLE model_route_group_memberships (
    tenant_id TEXT NOT NULL,
    route_group_id TEXT NOT NULL,
    model_route_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (route_group_id, model_route_id),
    FOREIGN KEY (tenant_id, route_group_id)
        REFERENCES route_groups(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, model_route_id)
        REFERENCES model_routes(tenant_id, id) ON DELETE CASCADE,
    CHECK (created_at >= 0)
);
CREATE INDEX model_route_group_memberships_tenant_group_idx
    ON model_route_group_memberships
       (tenant_id, route_group_id, model_route_id);
CREATE INDEX model_route_group_memberships_tenant_route_idx
    ON model_route_group_memberships
       (tenant_id, model_route_id, route_group_id);

-- Credential groups deliberately have no edge to routing_grants.  They are a
-- presentation-only classification and cannot widen or narrow authorization.
CREATE TABLE credential_groups (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (tenant_id, normalized_name),
    UNIQUE (tenant_id, id),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    CHECK (LENGTH(TRIM(name)) BETWEEN 1 AND 128),
    CHECK (LENGTH(normalized_name) BETWEEN 1 AND 128),
    CHECK (created_at >= 0),
    CHECK (updated_at >= created_at)
);

CREATE TABLE credential_group_memberships (
    tenant_id TEXT NOT NULL,
    credential_group_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (credential_group_id, key_id),
    FOREIGN KEY (tenant_id, credential_group_id)
        REFERENCES credential_groups(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, key_id)
        REFERENCES key_records(tenant_id, id) ON DELETE CASCADE,
    CHECK (created_at >= 0)
);
CREATE INDEX credential_group_memberships_tenant_group_idx
    ON credential_group_memberships
       (tenant_id, credential_group_id, key_id);
CREATE INDEX credential_group_memberships_tenant_key_idx
    ON credential_group_memberships
       (tenant_id, key_id, credential_group_id);

-- A grant targets exactly one route or one route group.  Separate partial
-- unique indexes are required because SQL UNIQUE constraints treat NULL values
-- as distinct on both supported databases.
CREATE TABLE routing_grants (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    model_route_id TEXT,
    route_group_id TEXT,
    created_at BIGINT NOT NULL,
    FOREIGN KEY (tenant_id, key_id)
        REFERENCES key_records(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, model_route_id)
        REFERENCES model_routes(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, route_group_id)
        REFERENCES route_groups(tenant_id, id) ON DELETE CASCADE,
    CHECK ((model_route_id IS NOT NULL) <> (route_group_id IS NOT NULL)),
    CHECK (created_at >= 0)
);
CREATE UNIQUE INDEX routing_grants_exact_route_unique
    ON routing_grants (tenant_id, key_id, model_route_id)
    WHERE model_route_id IS NOT NULL;
CREATE UNIQUE INDEX routing_grants_route_group_unique
    ON routing_grants (tenant_id, key_id, route_group_id)
    WHERE route_group_id IS NOT NULL;
CREATE INDEX routing_grants_tenant_key_idx
    ON routing_grants (tenant_id, key_id);
CREATE INDEX routing_grants_tenant_route_key_idx
    ON routing_grants (tenant_id, model_route_id, key_id)
    WHERE model_route_id IS NOT NULL;
CREATE INDEX routing_grants_tenant_group_key_idx
    ON routing_grants (tenant_id, route_group_id, key_id)
    WHERE route_group_id IS NOT NULL;

CREATE TABLE model_route_upstream_accounts (
    tenant_id TEXT NOT NULL,
    model_route_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    upstream_model TEXT NOT NULL,
    scheduling_weight BIGINT NOT NULL DEFAULT 100,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (model_route_id, upstream_account_id),
    FOREIGN KEY (tenant_id, model_route_id)
        REFERENCES model_routes(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, upstream_account_id)
        REFERENCES upstream_accounts(tenant_id, id) ON DELETE CASCADE,
    CHECK (LENGTH(TRIM(upstream_model)) BETWEEN 1 AND 255),
    CHECK (scheduling_weight BETWEEN 1 AND 1000000),
    CHECK (created_at >= 0)
);
CREATE INDEX model_route_upstream_accounts_tenant_route_idx
    ON model_route_upstream_accounts
       (tenant_id, model_route_id, upstream_account_id);
CREATE INDEX model_route_upstream_accounts_tenant_account_idx
    ON model_route_upstream_accounts
       (tenant_id, upstream_account_id, model_route_id);

CREATE TABLE model_route_included_provider_groups (
    tenant_id TEXT NOT NULL,
    model_route_id TEXT NOT NULL,
    provider_group_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (model_route_id, provider_group_id),
    FOREIGN KEY (tenant_id, model_route_id)
        REFERENCES model_routes(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, provider_group_id)
        REFERENCES provider_groups(tenant_id, id) ON DELETE CASCADE,
    CHECK (created_at >= 0)
);
CREATE INDEX model_route_included_provider_groups_tenant_route_idx
    ON model_route_included_provider_groups
       (tenant_id, model_route_id, provider_group_id);
CREATE INDEX model_route_included_provider_groups_tenant_group_idx
    ON model_route_included_provider_groups
       (tenant_id, provider_group_id, model_route_id);

CREATE TABLE model_route_excluded_provider_groups (
    tenant_id TEXT NOT NULL,
    model_route_id TEXT NOT NULL,
    provider_group_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (model_route_id, provider_group_id),
    FOREIGN KEY (tenant_id, model_route_id)
        REFERENCES model_routes(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, provider_group_id)
        REFERENCES provider_groups(tenant_id, id) ON DELETE CASCADE,
    CHECK (created_at >= 0)
);
CREATE INDEX model_route_excluded_provider_groups_tenant_route_idx
    ON model_route_excluded_provider_groups
       (tenant_id, model_route_id, provider_group_id);
CREATE INDEX model_route_excluded_provider_groups_tenant_group_idx
    ON model_route_excluded_provider_groups
       (tenant_id, provider_group_id, model_route_id);

-- Preserve existing routing exactly: the legacy route's account and concrete
-- model become its first explicit candidate.  This is idempotent for fixtures
-- that pre-populate the association before running the migration.
INSERT INTO model_route_upstream_accounts (
    tenant_id,
    model_route_id,
    upstream_account_id,
    upstream_model,
    scheduling_weight,
    created_at
)
SELECT tenant_id, id, upstream_account_id, upstream_model, 100, created_at
FROM model_routes
WHERE 1 = 1
ON CONFLICT (model_route_id, upstream_account_id) DO NOTHING;
