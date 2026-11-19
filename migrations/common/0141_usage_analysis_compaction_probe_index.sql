-- The usage page's output-rate metric (src/db/usage_analysis/output_rate.rs) cannot
-- use the usage_analysis_hourly/daily rollups: it has to re-evaluate conversation
-- compaction provenance on every read, because a late conversation projection must
-- be able to retroactively remove a request from the throughput series.
--
-- That forces it back onto request_stats_facts + request_records, and it excludes
-- compacted conversations with
--
--   NOT EXISTS (SELECT 1 FROM conversation_observations o
--                WHERE o.request_id = r.id AND o.key_id = r.key_id
--                  AND o.cluster_id = r.conversation_cluster_id
--                  AND o.compaction = 1)
--
-- conversation_observations_request_id_key can prove `request_id` matches, but the
-- remaining predicates live in the heap, so every probe still costs a heap fetch.
-- On the production deployment a seven-day window runs that anti-join ~110k times
-- at ~0.10 ms each (~11 s of the statement), and `compaction = 1` currently matches
-- zero rows -- all of that work proves an empty set.
--
-- A partial index restricted to compacted rows is effectively free to maintain
-- while nothing is compacted, and once compaction does start it keeps the probe
-- index-only: compaction, key_id and cluster_id are all answered from the key.
CREATE INDEX IF NOT EXISTS conversation_observations_compacted_probe_idx
    ON conversation_observations (request_id, key_id, cluster_id)
    WHERE compaction = 1;
