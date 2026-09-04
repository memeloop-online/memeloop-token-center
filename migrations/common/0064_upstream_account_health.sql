CREATE TABLE upstream_account_health (
    upstream_account_id TEXT PRIMARY KEY,
    consecutive_failures BIGINT NOT NULL DEFAULT 0,
    cooldown_until BIGINT NOT NULL DEFAULT 0,
    probe_lease_until BIGINT NOT NULL DEFAULT 0,
    last_failure_kind TEXT NOT NULL DEFAULT '',
    updated_at BIGINT NOT NULL,
    FOREIGN KEY (upstream_account_id) REFERENCES upstream_accounts(id) ON DELETE CASCADE
);

CREATE INDEX upstream_account_health_cooldown_idx
    ON upstream_account_health (cooldown_until, probe_lease_until, upstream_account_id);
