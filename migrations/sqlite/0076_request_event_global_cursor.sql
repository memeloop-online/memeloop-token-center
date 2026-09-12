CREATE INDEX IF NOT EXISTS request_events_global_cursor_idx
    ON request_events (event_at ASC, event_id ASC);
