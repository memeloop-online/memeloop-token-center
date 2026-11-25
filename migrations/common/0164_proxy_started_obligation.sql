ALTER TABLE request_records ADD COLUMN started_event_pending BIGINT NOT NULL DEFAULT 0;
CREATE INDEX request_records_started_pending ON request_records (created_at, id) WHERE started_event_pending = 1;
