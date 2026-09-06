-- Tenant identities are durable UUID roots. Lifecycle changes are explicit
-- control-plane operations, never an implicit side effect of credential or
-- route creation. The default keeps existing installations immediately active.
ALTER TABLE tenants ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
    CHECK (status IN ('active', 'archived'));
ALTER TABLE tenants ADD COLUMN updated_at BIGINT NOT NULL DEFAULT 0;
UPDATE tenants SET updated_at = created_at WHERE updated_at = 0;

-- The audit deliberately does not reference `tenants`: it remains available
-- after an archived tenant is deleted and records only non-secret IDs.
CREATE TABLE tenant_lifecycle_audit (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    external_id TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('created', 'renamed', 'archived', 'restored', 'deleted')),
    actor_service_id TEXT,
    created_at BIGINT NOT NULL
);
CREATE INDEX tenant_lifecycle_audit_tenant_created_idx
    ON tenant_lifecycle_audit (tenant_id, created_at DESC, id DESC);
