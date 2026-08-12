CREATE TABLE IF NOT EXISTS plugin_kv (
    plugin_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value BYTEA NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (plugin_id, key)
);
