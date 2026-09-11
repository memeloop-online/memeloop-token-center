CREATE TABLE upstream_transport_proxy_audit (
    id TEXT PRIMARY KEY,
    upstream_account_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    credential_generation BIGINT NOT NULL CHECK (credential_generation > 0),
    previous_fingerprint TEXT,
    previous_scheme TEXT CHECK (previous_scheme IS NULL OR previous_scheme IN ('socks5', 'socks5h')),
    new_fingerprint TEXT NOT NULL,
    new_scheme TEXT NOT NULL CHECK (new_scheme IN ('socks5', 'socks5h')),
    remote_dns BOOLEAN NOT NULL,
    actor_service_id TEXT,
    operator_is_bootstrap BOOLEAN NOT NULL,
    created_at BIGINT NOT NULL,
    CHECK (
        (operator_is_bootstrap = TRUE AND actor_service_id IS NULL)
        OR (operator_is_bootstrap = FALSE AND actor_service_id IS NOT NULL)
    )
);

CREATE INDEX upstream_transport_proxy_audit_account_created_idx
    ON upstream_transport_proxy_audit (upstream_account_id, created_at DESC, id DESC);

CREATE INDEX upstream_transport_proxy_audit_tenant_created_idx
    ON upstream_transport_proxy_audit (tenant_id, created_at DESC, id DESC);
