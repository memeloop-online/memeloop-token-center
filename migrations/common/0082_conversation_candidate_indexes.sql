CREATE INDEX IF NOT EXISTS conversation_observations_key_response_time_idx
    ON conversation_observations (key_id, upstream_response_id, created_at DESC)
    WHERE upstream_response_id IS NOT NULL;
