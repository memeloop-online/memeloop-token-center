-- Correlated latest-request lookups must be able to seek directly to either a
-- linked conversation id or the NULL unlinked bucket. A non-partial index also
-- avoids making the planner prove a partial-index predicate about an outer CTE.
CREATE INDEX IF NOT EXISTS request_records_session_latest_idx
    ON request_records
       (key_id ASC, conversation_cluster_id ASC, created_at DESC, id DESC);
