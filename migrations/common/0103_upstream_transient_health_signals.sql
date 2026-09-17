CREATE TABLE upstream_account_transient_health_signals (
    upstream_account_id TEXT NOT NULL PRIMARY KEY,
    credential_generation BIGINT NOT NULL,
    sample_count BIGINT NOT NULL DEFAULT 0 CHECK (sample_count >= 0),
    ewma_micros BIGINT NOT NULL DEFAULT 0 CHECK (ewma_micros >= 0 AND ewma_micros <= 1000000),
    last_observed_at BIGINT NOT NULL DEFAULT 0,
    recovery_successes BIGINT NOT NULL DEFAULT 0 CHECK (recovery_successes >= 0),
    revision BIGINT NOT NULL DEFAULT 0 CHECK (revision >= 0),
    FOREIGN KEY (upstream_account_id) REFERENCES upstream_accounts(id) ON DELETE CASCADE
);

CREATE INDEX upstream_account_transient_health_signals_observed_idx
    ON upstream_account_transient_health_signals (last_observed_at, upstream_account_id);
