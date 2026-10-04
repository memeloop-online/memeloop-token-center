CREATE TABLE upstream_transport_proxy_selections (
    account_id TEXT PRIMARY KEY REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    group_version BIGINT NOT NULL CHECK (group_version > 0),
    group_fingerprint TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    base_index BIGINT NOT NULL CHECK (base_index BETWEEN 0 AND 3),
    selected_index BIGINT NOT NULL CHECK (selected_index BETWEEN 0 AND 3),
    selection_generation BIGINT NOT NULL CHECK (selection_generation > 0)
);
