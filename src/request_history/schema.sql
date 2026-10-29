CREATE TABLE IF NOT EXISTS request_history_v1
(
    tenant_id UUID,
    key_id UUID,
    created_at Int64,
    request_id UUID,
    source LowCardinality(String),
    revision UInt64,
    deleted UInt8,
    snapshot_hash FixedString(64),
    model String,
    protocol LowCardinality(String),
    outcome LowCardinality(String),
    filter_status LowCardinality(String),
    stats_status LowCardinality(String),
    filter_cost_micros Nullable(Int64),
    settlement_state LowCardinality(String),
    error_code Nullable(String),
    upstream_account_id Nullable(UUID),
    route_id Nullable(UUID),
    duration_ms Nullable(Int64),
    cost_micros Nullable(Int64),
    currency Nullable(String),
    input_tokens Nullable(Int64),
    output_tokens Nullable(Int64),
    cached_input_tokens Nullable(Int64),
    cache_write_tokens Nullable(Int64),
    generation_units Nullable(Int64),
    billing_unit Nullable(String),
    key_alias Nullable(String),
    principal Nullable(String),
    session_id Nullable(String),
    task_kind Nullable(String),
    snapshot String
)
ENGINE = ReplacingMergeTree(revision)
PARTITION BY intDiv(created_at, 2678400000)
ORDER BY (tenant_id, key_id, created_at, request_id, source);
