ALTER TABLE conversation_observations
    ADD COLUMN upstream_response_id TEXT;
CREATE INDEX IF NOT EXISTS conversation_observations_upstream_response_idx
    ON conversation_observations (upstream_response_id)
    WHERE upstream_response_id IS NOT NULL;
