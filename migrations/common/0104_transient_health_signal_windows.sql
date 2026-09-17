ALTER TABLE upstream_account_transient_health_signals
    ADD COLUMN transient_window_ms BIGINT NOT NULL DEFAULT 60000
        CHECK (transient_window_ms >= 1000 AND transient_window_ms <= 300000);

ALTER TABLE upstream_account_transient_health_signals
    ADD COLUMN window_started_at BIGINT NOT NULL DEFAULT 0
        CHECK (window_started_at >= 0);
