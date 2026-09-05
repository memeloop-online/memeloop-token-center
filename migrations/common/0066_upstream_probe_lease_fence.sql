ALTER TABLE upstream_account_health
    ADD COLUMN probe_lease_token TEXT NOT NULL DEFAULT '';
