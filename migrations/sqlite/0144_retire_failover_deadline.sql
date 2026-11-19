UPDATE upstream_accounts
SET config_json = json_remove(config_json, '$.transport_policy.failover_deadline_millis')
WHERE json_type(config_json, '$.transport_policy') = 'object'
  AND json_type(config_json, '$.transport_policy.failover_deadline_millis') IS NOT NULL;
