-- Global runtime authority. Package roots and host grants are never API data.
CREATE TABLE application_plugin_candidates (
    inventory_id TEXT PRIMARY KEY,
    identity_digest TEXT NOT NULL,
    contract_digest TEXT NOT NULL,
    created_at BIGINT NOT NULL
);
CREATE TABLE application_plugin_revisions (
    revision BIGINT PRIMARY KEY CHECK (revision > 0),
    inventory_id TEXT NOT NULL REFERENCES application_plugin_candidates(inventory_id),
    reason TEXT NOT NULL CHECK (reason IN ('initial', 'reload', 'rollback')),
    created_at BIGINT NOT NULL
);
CREATE TABLE application_plugin_head (
    scope TEXT PRIMARY KEY CHECK (scope = 'global'),
    revision BIGINT NOT NULL REFERENCES application_plugin_revisions(revision)
);
CREATE TABLE application_plugin_operations (
    idempotency_key TEXT PRIMARY KEY,
    request_hash TEXT NOT NULL,
    result_revision BIGINT REFERENCES application_plugin_revisions(revision),
    created_at BIGINT NOT NULL
);
