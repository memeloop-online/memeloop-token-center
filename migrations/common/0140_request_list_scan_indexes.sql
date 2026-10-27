-- The operator request list merges three independently bounded sources, ordered
-- newest-first, before applying its page limit.  Two of them already expose that
-- ordering key:
--   * request_records      -> request_records_recent_idx (created_at DESC, id DESC)
--   * generation_jobs      -> generation_jobs_global_time_idx (created_at DESC, id DESC)
-- The unlinked-archive source had no global newest-first key, so every global
-- operator page load scanned and sorted the whole table to keep the newest 101
-- rows.  This key lets the planner walk the ordered rows directly and stop at the
-- page boundary.  (A tenant carries the same ordering through the existing
-- key/cluster page index when it is scoped to one credential.)
CREATE INDEX IF NOT EXISTS session_archive_unlinked_recent_idx
    ON session_archive_unlinked_requests (source_started_at DESC, archive_request_id DESC);

-- Every request-list page resolves the visible page's egress observations with
-- `WHERE request_id IN (...)` and a window over the same rows.  The only prior
-- key was (observed_at, request_id), which cannot serve a request_id lookup, so
-- the page seq-scanned the whole table on each load.  A request_id-leading key
-- turns that into one bounded index lookup per visible request.
CREATE INDEX IF NOT EXISTS request_egress_attempts_request_idx
    ON request_egress_attempts (request_id);
