-- PostgreSQL-only, low-lock index migration used by
-- ops/backfill-postgres-history-partitions.sh.
--
-- ON ONLY creates partitioned-index metadata without building every leaf
-- while holding a parent-table lock.  The shell driver builds any missing leaf
-- indexes with CREATE INDEX CONCURRENTLY, then attaches them.  Existing valid
-- application indexes are retained and merely verified/attached.

CREATE INDEX IF NOT EXISTS request_records_recent_idx
    ON ONLY public.request_records (created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS request_records_tenant_time_idx
    ON ONLY public.request_records (tenant_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS request_records_key_time_idx
    ON ONLY public.request_records (key_id, created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS request_events_global_cursor_idx
    ON ONLY public.request_events (event_at ASC, event_id ASC);
CREATE INDEX IF NOT EXISTS request_events_tenant_cursor_idx
    ON ONLY public.request_events (tenant_id, event_at ASC, event_id ASC);

COMMENT ON INDEX public.request_records_recent_idx IS
    'Global newest-request access path; leaf indexes are installed concurrently by the history backfill operator.';
COMMENT ON INDEX public.request_events_global_cursor_idx IS
    'Global request-event cursor access path; leaf indexes are installed concurrently by the history backfill operator.';
