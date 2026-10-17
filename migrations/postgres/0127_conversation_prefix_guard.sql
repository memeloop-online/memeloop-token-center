CREATE OR REPLACE FUNCTION enforce_conversation_prefix_format() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE value jsonb;
BEGIN
    value := NEW.atom_hashes_json::jsonb;
    IF jsonb_typeof(value) <> 'object'
       OR NOT (value ? 'context_prefix_v1' AND value ? 'atom_count')
       OR (SELECT COUNT(*) FROM jsonb_object_keys(value)) <> 2
       OR jsonb_typeof(value->'atom_count') <> 'number'
       OR (value->>'atom_count') !~ '^(0|[1-9][0-9]{0,3})$'
       OR (value->>'atom_count')::BIGINT > 1024
       OR NOT ((value->>'atom_count' = '0' AND value->'context_prefix_v1' = 'null'::jsonb)
          OR ((value->>'atom_count')::BIGINT > 0 AND jsonb_typeof(value->'context_prefix_v1') = 'string'
              AND value->>'context_prefix_v1' ~ '^[0-9a-f]{64}$')) THEN
        RAISE EXCEPTION 'conversation observation requires context-prefix-v1' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
DROP TRIGGER IF EXISTS conversation_prefix_format ON conversation_observations;
CREATE TRIGGER conversation_prefix_format BEFORE INSERT OR UPDATE OF atom_hashes_json ON conversation_observations
FOR EACH ROW EXECUTE FUNCTION enforce_conversation_prefix_format();
