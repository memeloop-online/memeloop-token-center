-- A route is now a reusable rule with a candidate set.  Multiple routes may
-- intentionally expose the same public model at the same priority.
ALTER TABLE model_routes
    DROP CONSTRAINT IF EXISTS model_routes_tenant_id_public_model_protocol_priority_key;
