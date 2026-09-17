CREATE TABLE group_routing_v2_transient_health_signals (
    upstream_account_id TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    policy_scope TEXT NOT NULL CHECK (LENGTH(policy_scope) = 64),
    transient_window_ms BIGINT NOT NULL
        CHECK (transient_window_ms >= 1000 AND transient_window_ms <= 300000),
    window_started_at BIGINT NOT NULL CHECK (window_started_at >= 0),
    sample_count BIGINT NOT NULL DEFAULT 0 CHECK (sample_count >= 0),
    ewma_micros BIGINT NOT NULL DEFAULT 0
        CHECK (ewma_micros >= 0 AND ewma_micros <= 1000000),
    last_observed_at BIGINT NOT NULL DEFAULT 0,
    recovery_successes BIGINT NOT NULL DEFAULT 0 CHECK (recovery_successes >= 0),
    revision BIGINT NOT NULL DEFAULT 0 CHECK (revision >= 0),
    PRIMARY KEY (upstream_account_id, credential_generation, policy_scope),
    FOREIGN KEY (upstream_account_id) REFERENCES upstream_accounts(id) ON DELETE CASCADE
);

CREATE INDEX group_routing_v2_transient_health_signals_observed_idx
    ON group_routing_v2_transient_health_signals
       (last_observed_at, upstream_account_id);
