ALTER TABLE provider_groups ADD COLUMN routing_strategy TEXT;
ALTER TABLE provider_groups ADD COLUMN routing_priority INTEGER NOT NULL DEFAULT 0;
ALTER TABLE provider_groups ADD COLUMN strategy_version BIGINT NOT NULL DEFAULT 0 CHECK (strategy_version >= 0);
ALTER TABLE route_groups ADD COLUMN routing_strategy TEXT;
ALTER TABLE route_groups ADD COLUMN routing_priority INTEGER NOT NULL DEFAULT 0;
ALTER TABLE route_groups ADD COLUMN strategy_version BIGINT NOT NULL DEFAULT 0 CHECK (strategy_version >= 0);
