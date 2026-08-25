ALTER TABLE conversation_observations ADD COLUMN session_name TEXT;
ALTER TABLE conversation_observations ADD COLUMN trace_id TEXT;
ALTER TABLE conversation_observations ADD COLUMN span_id TEXT;
ALTER TABLE conversation_observations ADD COLUMN parent_span_id TEXT;
ALTER TABLE conversation_observations ADD COLUMN agent_id TEXT;
ALTER TABLE conversation_observations ADD COLUMN parent_agent_id TEXT;
ALTER TABLE conversation_observations ADD COLUMN task_kind TEXT;
ALTER TABLE conversation_observations ADD COLUMN labels_json TEXT NOT NULL DEFAULT '{}';
ALTER TABLE conversation_observations ADD COLUMN metadata_source TEXT;

CREATE INDEX conversation_observations_trace_idx
    ON conversation_observations (key_id, trace_id, created_at DESC)
    WHERE trace_id IS NOT NULL;

CREATE INDEX conversation_observations_agent_idx
    ON conversation_observations (key_id, agent_id, created_at DESC)
    WHERE agent_id IS NOT NULL;
