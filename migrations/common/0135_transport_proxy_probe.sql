ALTER TABLE transport_proxy_groups ADD COLUMN probe_url TEXT;
ALTER TABLE transport_proxy_groups ADD COLUMN probe_attempts BIGINT NOT NULL DEFAULT 2;
