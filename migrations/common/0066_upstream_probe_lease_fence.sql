ALTER TABLE upstream_account_health
    ADD COLUMN probe_lease_token TEXT NOT NULL DEFAULT '';

ALTER TABLE upstream_account_health
    ADD COLUMN credential_generation BIGINT NOT NULL DEFAULT 0;
