CREATE TABLE upstream_transport_proxy_selections_expanded (
    account_id TEXT PRIMARY KEY REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    group_version BIGINT NOT NULL CHECK (group_version > 0),
    group_fingerprint TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    base_index BIGINT NOT NULL CHECK (base_index >= 0),
    selected_index BIGINT NOT NULL CHECK (selected_index >= 0),
    selection_generation BIGINT NOT NULL CHECK (selection_generation > 0)
);

INSERT INTO upstream_transport_proxy_selections_expanded
    (account_id, group_version, group_fingerprint, credential_generation,
     base_index, selected_index, selection_generation)
SELECT account_id, group_version, group_fingerprint, credential_generation,
       base_index, selected_index, selection_generation
FROM upstream_transport_proxy_selections;

DROP TABLE upstream_transport_proxy_selections;
ALTER TABLE upstream_transport_proxy_selections_expanded RENAME TO upstream_transport_proxy_selections;
