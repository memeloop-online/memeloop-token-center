-- Synchronous non-token media requests share request lifecycle durability, but
-- their billable units must never occupy token columns in analytics facts.
ALTER TABLE request_stats_facts
    ADD COLUMN generation_units BIGINT NOT NULL DEFAULT 0;
