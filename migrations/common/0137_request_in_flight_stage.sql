-- Live proxy stage for an open request. Null means no stage was recorded.
-- Existing settlement rows are left untouched.
ALTER TABLE request_records ADD COLUMN in_flight_stage TEXT;
