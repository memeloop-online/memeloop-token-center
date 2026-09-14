CREATE TABLE application_plugin_installations (
    id TEXT PRIMARY KEY,
    idempotency_hash TEXT NOT NULL UNIQUE,
    request_hash TEXT NOT NULL,
    inventory_id TEXT NOT NULL UNIQUE,
    packages_json TEXT NOT NULL,
    actor TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('installing', 'review', 'registered', 'failed')),
    review_digest TEXT,
    review_json TEXT,
    checkpoints_json TEXT NOT NULL DEFAULT '{}',
    failure_category TEXT,
    lease_until BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE TABLE application_plugin_install_lock (
    scope TEXT PRIMARY KEY CHECK (scope = 'global'),
    operation_id TEXT NOT NULL,
    lease_until BIGINT NOT NULL
);
CREATE TABLE application_plugin_audit (
    id TEXT PRIMARY KEY,
    event_key TEXT NOT NULL UNIQUE,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    inventory_id TEXT,
    revision BIGINT,
    outcome TEXT NOT NULL,
    created_at BIGINT NOT NULL
);
CREATE INDEX application_plugin_audit_page ON application_plugin_audit(created_at DESC, id DESC);
