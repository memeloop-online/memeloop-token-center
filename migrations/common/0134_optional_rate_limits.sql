ALTER TABLE usage_reservations
    ADD COLUMN tpm_window_applied BIGINT NOT NULL DEFAULT 1
    CHECK (tpm_window_applied IN (0, 1));
