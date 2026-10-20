CREATE INDEX proxy_operational_terminals_time_idx
    ON proxy_operational_terminals (request_created_at, tenant_id, key_id, request_id);
