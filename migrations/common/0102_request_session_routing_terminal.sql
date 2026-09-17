ALTER TABLE request_records ADD COLUMN explicit_session_id TEXT;

CREATE INDEX IF NOT EXISTS request_records_session_routing_terminal_idx
    ON request_records
       (tenant_id, key_id, explicit_session_id, model, protocol, completed_at DESC, id DESC)
    WHERE explicit_session_id IS NOT NULL AND completed_at IS NOT NULL;
