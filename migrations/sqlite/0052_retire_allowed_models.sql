-- Routing grants were frozen at the v43 boundary. Retire the legacy policy
-- source so it cannot accidentally become a second authorization authority.
-- json_remove/json_type fail the migration on malformed historical JSON.
UPDATE key_records
SET policy_json = json_remove(policy_json, '$.allowed_models')
WHERE json_type(policy_json, '$.allowed_models') IS NOT NULL;
