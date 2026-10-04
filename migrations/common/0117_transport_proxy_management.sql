CREATE TABLE transport_proxy_management_lock (
    id BIGINT PRIMARY KEY CHECK (id = 1),
    revision BIGINT NOT NULL
);
INSERT INTO transport_proxy_management_lock (id, revision) VALUES (1, 0);

CREATE TABLE transport_proxy_groups (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    version BIGINT NOT NULL CHECK (version > 0),
    members_ciphertext TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (tenant_id, name)
);

CREATE TABLE transport_proxy_bindings (
    account_id TEXT PRIMARY KEY REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    group_id TEXT REFERENCES transport_proxy_groups(id),
    version BIGINT NOT NULL CHECK (version > 0),
    initial_member_id TEXT,
    replacement_member_id TEXT,
    updated_at BIGINT NOT NULL
);
CREATE INDEX transport_proxy_bindings_group ON transport_proxy_bindings(group_id);

CREATE TABLE transport_proxy_management_audit (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    resource_id TEXT NOT NULL,
    action TEXT NOT NULL,
    version BIGINT NOT NULL,
    actor_service_id TEXT,
    created_at BIGINT NOT NULL
);
