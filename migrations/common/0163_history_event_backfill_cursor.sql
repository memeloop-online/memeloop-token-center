-- Event history follows its partition key and existing global cursor index.
-- Preserve the old UUID-only watermark for the one-time compatible scan.
ALTER TABLE request_history_backfill ADD COLUMN after_created_at BIGINT;
ALTER TABLE request_history_backfill ADD COLUMN legacy_after_id TEXT;
