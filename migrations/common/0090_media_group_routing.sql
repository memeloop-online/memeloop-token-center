ALTER TABLE generation_jobs ADD COLUMN routing_snapshot TEXT;
ALTER TABLE request_records ADD COLUMN routing_snapshot TEXT;
ALTER TABLE request_records ADD COLUMN submission_started_at BIGINT;
ALTER TABLE request_records ADD COLUMN submission_uncertain_at BIGINT;
