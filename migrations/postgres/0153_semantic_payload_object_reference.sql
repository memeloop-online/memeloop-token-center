-- Metadata-only widening: existing rows were already constrained to v1.
-- NOT VALID avoids scanning/decompressing historical PG bodies during rollout;
-- PostgreSQL still enforces the new condition on every subsequent write.
ALTER TABLE conversation_semantic_payloads
    DROP CONSTRAINT conversation_semantic_payloads_format_version_check,
    ADD CONSTRAINT conversation_semantic_payloads_format_version_check
        CHECK (format_version IN (1, 2)) NOT VALID;
