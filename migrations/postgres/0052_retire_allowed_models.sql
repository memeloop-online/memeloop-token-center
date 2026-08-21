-- Routing grants were frozen at the v43 boundary. Retire the legacy policy
-- source so it cannot accidentally become a second authorization authority.
-- The jsonb cast fails the migration on malformed historical JSON.
UPDATE key_records
SET policy_json = ((policy_json::jsonb - 'allowed_models')::TEXT)
WHERE policy_json::jsonb ? 'allowed_models';
