UPDATE upstream_accounts
SET config_json = (config_json::jsonb #- '{transport_policy,failover_deadline_millis}')::text
WHERE jsonb_typeof(config_json::jsonb -> 'transport_policy') = 'object'
  AND (config_json::jsonb -> 'transport_policy') ? 'failover_deadline_millis';
