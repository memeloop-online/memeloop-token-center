-- Bind source receipts to their authoritative read model and activation epoch.
-- Existing receipts remain evidence but require a new bound baseline for cutover.
ALTER TABLE request_history_baseline_runs ADD COLUMN read_model_version BIGINT;
ALTER TABLE request_history_baseline_runs ADD COLUMN activation_epoch BIGINT;
ALTER TABLE request_history_baseline_runs ADD COLUMN chunk_count BIGINT;
ALTER TABLE request_history_baseline_runs ADD COLUMN manifest_hash TEXT;
ALTER TABLE request_history_baseline_runs ADD COLUMN sources_complete TEXT;
ALTER TABLE request_history_baseline_runs ADD COLUMN parity_json TEXT;
