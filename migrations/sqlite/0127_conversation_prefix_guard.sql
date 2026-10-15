CREATE TRIGGER IF NOT EXISTS conversation_prefix_format_insert BEFORE INSERT ON conversation_observations
BEGIN
    SELECT CASE WHEN COALESCE((
        json_valid(NEW.atom_hashes_json)
        AND json_type(NEW.atom_hashes_json) = 'object'
        AND (SELECT COUNT(*) FROM json_each(NEW.atom_hashes_json)) = 2
        AND json_type(NEW.atom_hashes_json, '$.atom_count') = 'integer'
        AND json_extract(NEW.atom_hashes_json, '$.atom_count') BETWEEN 0 AND 1024
        AND ((json_extract(NEW.atom_hashes_json, '$.atom_count') = 0 AND json_type(NEW.atom_hashes_json, '$.context_prefix_v1') = 'null')
          OR (json_extract(NEW.atom_hashes_json, '$.atom_count') > 0 AND json_type(NEW.atom_hashes_json, '$.context_prefix_v1') = 'text'
              AND length(json_extract(NEW.atom_hashes_json, '$.context_prefix_v1')) = 64
              AND json_extract(NEW.atom_hashes_json, '$.context_prefix_v1') NOT GLOB '*[^0-9a-f]*'))
    ), 0) = 0 THEN RAISE(ABORT, 'conversation observation requires context-prefix-v1') END;
END;
CREATE TRIGGER IF NOT EXISTS conversation_prefix_format_update BEFORE UPDATE OF atom_hashes_json ON conversation_observations
BEGIN
    SELECT CASE WHEN COALESCE((
        json_valid(NEW.atom_hashes_json)
        AND json_type(NEW.atom_hashes_json) = 'object'
        AND (SELECT COUNT(*) FROM json_each(NEW.atom_hashes_json)) = 2
        AND json_type(NEW.atom_hashes_json, '$.atom_count') = 'integer'
        AND json_extract(NEW.atom_hashes_json, '$.atom_count') BETWEEN 0 AND 1024
        AND ((json_extract(NEW.atom_hashes_json, '$.atom_count') = 0 AND json_type(NEW.atom_hashes_json, '$.context_prefix_v1') = 'null')
          OR (json_extract(NEW.atom_hashes_json, '$.atom_count') > 0 AND json_type(NEW.atom_hashes_json, '$.context_prefix_v1') = 'text'
              AND length(json_extract(NEW.atom_hashes_json, '$.context_prefix_v1')) = 64
              AND json_extract(NEW.atom_hashes_json, '$.context_prefix_v1') NOT GLOB '*[^0-9a-f]*'))
    ), 0) = 0 THEN RAISE(ABORT, 'conversation observation requires context-prefix-v1') END;
END;
