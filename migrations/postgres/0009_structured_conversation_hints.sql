ALTER TABLE conversation_observations ADD COLUMN turn_id TEXT;
ALTER TABLE conversation_observations ADD COLUMN parent_turn_id TEXT;
ALTER TABLE conversation_observations ADD COLUMN branch_id TEXT;
ALTER TABLE conversation_observations ADD COLUMN compaction BIGINT NOT NULL DEFAULT 0;

CREATE INDEX conversation_observations_turn_idx
    ON conversation_observations (turn_id, created_at DESC)
    WHERE turn_id IS NOT NULL;
CREATE INDEX conversation_observations_session_idx
    ON conversation_observations (explicit_session_id, created_at DESC)
    WHERE explicit_session_id IS NOT NULL;
