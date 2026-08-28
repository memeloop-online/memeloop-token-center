CREATE TABLE IF NOT EXISTS cpamp_import_checkpoints (
  tenant_external_id text NOT NULL,
  source text NOT NULL,
  watermark_ms bigint NOT NULL,
  watermark_hash text NOT NULL,
  imported_events bigint NOT NULL,
  updated_at bigint NOT NULL,
  PRIMARY KEY (tenant_external_id, source)
);
ALTER TABLE cpamp_import_checkpoints
  ADD COLUMN IF NOT EXISTS tenant_external_id text;
UPDATE cpamp_import_checkpoints
   SET tenant_external_id = :'tenant_external_id'
 WHERE tenant_external_id IS NULL;
ALTER TABLE cpamp_import_checkpoints
  ALTER COLUMN tenant_external_id SET NOT NULL;
ALTER TABLE cpamp_import_checkpoints
  DROP CONSTRAINT IF EXISTS cpamp_import_checkpoints_pkey;
ALTER TABLE cpamp_import_checkpoints
  ADD CONSTRAINT cpamp_import_checkpoints_pkey PRIMARY KEY (tenant_external_id, source);
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_usage (
  event_hash text, request_id text, timestamp_ms bigint, provider text,
  model text, endpoint text, api_key_hash text, input_tokens bigint,
  output_tokens bigint, latency_ms bigint, failed bigint,
  fail_status_code bigint, fail_summary text
);
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_aliases (
  api_key_hash text, alias text, updated_at_ms bigint
);
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_prices (
  model text, prompt_per_1m double precision, completion_per_1m double precision,
  source text, updated_at_ms bigint
);
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_identities (
  api_key_hash text PRIMARY KEY, key_id text NOT NULL, account_id text NOT NULL,
  alias text NOT NULL
);
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_new_requests
  (LIKE request_records INCLUDING DEFAULTS);
TRUNCATE cpamp_import_usage, cpamp_import_aliases, cpamp_import_prices,
         cpamp_import_identities, cpamp_import_new_requests;
