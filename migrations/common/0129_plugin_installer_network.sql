CREATE TABLE plugin_installer_network_preferences (
    actor TEXT PRIMARY KEY,
    preference_ciphertext TEXT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE TABLE plugin_installer_network_snapshots (
    operation_key TEXT PRIMARY KEY,
    actor TEXT NOT NULL,
    selection_digest TEXT NOT NULL,
    snapshot_ciphertext TEXT NOT NULL,
    created_at BIGINT NOT NULL
);
