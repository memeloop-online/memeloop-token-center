-- Canonicalize the first-party OpenAI Audio route protocol before the server
-- starts enforcing it. This is a one-way compatibility migration: runtime
-- routing only accepts `openai-audio` after this schema version is applied.
UPDATE model_routes
   SET protocol = 'openai-audio'
 WHERE protocol = 'audio';

-- Synchronous media requests have provider-defined billing units. Keep those
-- quantities separate from token admission, accounting, and audit columns.
ALTER TABLE usage_reservations
    ADD COLUMN reserved_units BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_reservations
    ADD COLUMN billing_unit TEXT NOT NULL DEFAULT '';
ALTER TABLE usage_reservations
    ADD COLUMN micros_per_unit BIGINT NOT NULL DEFAULT 0;

ALTER TABLE request_records
    ADD COLUMN billed_units BIGINT NOT NULL DEFAULT 0;
ALTER TABLE request_records
    ADD COLUMN billing_unit TEXT NOT NULL DEFAULT '';

ALTER TABLE request_stats_facts
    ADD COLUMN billing_unit TEXT NOT NULL DEFAULT '';

-- Repair rows written by the preview implementation, which temporarily used
-- output-token columns as a compatibility carrier for audio seconds.
UPDATE usage_reservations
   SET reserved_units = reserved_tokens,
       billing_unit = 'second',
       reserved_tokens = 0
 WHERE id IN (
       SELECT reservation_id
         FROM request_records
        WHERE protocol = 'audio-transcription'
   );

-- Remove only the legacy audio reservations from their original minute
-- buckets. Requests and non-audio token quantities in the same window remain
-- untouched; defensive clamping handles already-corrected or partial history.
UPDATE rate_limit_windows
   SET tokens = CASE
       WHEN tokens < COALESCE((
           SELECT SUM(CASE WHEN u.status = 'reserved' THEN u.reserved_units ELSE r.output_tokens END)
             FROM usage_reservations u
             JOIN request_records r ON r.reservation_id = u.id
            WHERE r.protocol = 'audio-transcription'
              AND r.key_id = rate_limit_windows.key_id
              AND (r.created_at / 60000) * 60000 = rate_limit_windows.window_start
       ), 0) THEN 0
       ELSE tokens - COALESCE((
           SELECT SUM(CASE WHEN u.status = 'reserved' THEN u.reserved_units ELSE r.output_tokens END)
             FROM usage_reservations u
             JOIN request_records r ON r.reservation_id = u.id
            WHERE r.protocol = 'audio-transcription'
              AND r.key_id = rate_limit_windows.key_id
              AND (r.created_at / 60000) * 60000 = rate_limit_windows.window_start
       ), 0)
       END
 WHERE EXISTS (
       SELECT 1
         FROM usage_reservations u
         JOIN request_records r ON r.reservation_id = u.id
        WHERE r.protocol = 'audio-transcription'
          AND r.key_id = rate_limit_windows.key_id
          AND (r.created_at / 60000) * 60000 = rate_limit_windows.window_start
   );

UPDATE request_records
   SET billed_units = output_tokens,
       billing_unit = 'second',
       input_tokens = 0,
       cached_input_tokens = 0,
       cache_write_tokens = 0,
       output_tokens = 0
 WHERE protocol = 'audio-transcription';

UPDATE request_stats_facts
   SET generation_units = output_tokens,
       billing_unit = 'second',
       input_tokens = 0,
       cached_input_tokens = 0,
       cache_write_tokens = 0,
       output_tokens = 0
 WHERE protocol = 'audio-transcription';
