CREATE TABLE application_plugin_release_resolutions (
    id TEXT PRIMARY KEY,
    operation_key TEXT NOT NULL UNIQUE,
    actor TEXT NOT NULL,
    release_url TEXT NOT NULL,
    descriptor_sha256 TEXT NOT NULL,
    resolved_json TEXT NOT NULL,
    install_key_hash TEXT UNIQUE,
    created_at BIGINT NOT NULL
);
