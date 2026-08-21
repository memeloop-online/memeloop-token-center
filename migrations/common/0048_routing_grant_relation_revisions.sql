-- Grant edges can be replaced from either the credential editor or the model
-- route editor.  A revision belongs to the relation set, rather than to the
-- credential/route metadata row, so unrelated metadata updates do not make a
-- grant editor stale (and grant updates cannot silently overwrite each other).
CREATE TABLE routing_grant_relation_revisions (
    tenant_id TEXT NOT NULL,
    subject_kind TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    key_id TEXT,
    model_route_id TEXT,
    revision BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, subject_kind, subject_id),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, key_id)
        REFERENCES key_records(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, model_route_id)
        REFERENCES model_routes(tenant_id, id) ON DELETE CASCADE,
    CHECK (
        (subject_kind = 'credential'
            AND subject_id = key_id
            AND key_id IS NOT NULL
            AND model_route_id IS NULL)
        OR
        (subject_kind = 'route'
            AND subject_id = model_route_id
            AND key_id IS NULL
            AND model_route_id IS NOT NULL)
    ),
    CHECK (revision >= 0)
);

-- This short-lived per-tenant row lock makes discovery of the old reverse
-- edges and locking/bumping their relation revisions portable across both
-- PostgreSQL and SQLite.  Grant writes are control-plane operations, so
-- serializing only these writes per tenant does not affect gateway traffic.
CREATE TABLE routing_relation_write_locks (
    tenant_id TEXT PRIMARY KEY,
    generation BIGINT NOT NULL DEFAULT 0,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    CHECK (generation >= 0)
);

INSERT INTO routing_relation_write_locks (tenant_id, generation)
SELECT id, 0 FROM tenants
WHERE 1 = 1
ON CONFLICT (tenant_id) DO NOTHING;

INSERT INTO routing_grant_relation_revisions (
    tenant_id,
    subject_kind,
    subject_id,
    key_id,
    model_route_id,
    revision
)
SELECT tenant_id, 'credential', id, id, NULL, 0
FROM key_records
WHERE 1 = 1
ON CONFLICT (tenant_id, subject_kind, subject_id) DO NOTHING;

INSERT INTO routing_grant_relation_revisions (
    tenant_id,
    subject_kind,
    subject_id,
    key_id,
    model_route_id,
    revision
)
SELECT tenant_id, 'route', id, NULL, id, 0
FROM model_routes
WHERE 1 = 1
ON CONFLICT (tenant_id, subject_kind, subject_id) DO NOTHING;
