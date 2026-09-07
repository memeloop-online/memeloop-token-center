-- A top upstream/model snapshot first ranks compact usage rollups, then reads
-- at most five terminal outcomes for each selected stable pair. These indexes
-- make that drilldown an ordered, bounded fact lookup instead of a scan of
-- request_records or generation_jobs.
CREATE INDEX IF NOT EXISTS request_stats_facts_monitoring_outcome_idx
    ON request_stats_facts (upstream_account_id, model, created_at DESC, request_id DESC)
    WHERE upstream_account_id <> '';

CREATE INDEX IF NOT EXISTS generation_stats_facts_monitoring_outcome_idx
    ON generation_stats_facts (upstream_account_id, model, created_at DESC, job_id DESC)
    WHERE upstream_account_id <> '';
