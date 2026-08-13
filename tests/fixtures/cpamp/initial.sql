CREATE TABLE usage_events (
  event_hash TEXT NOT NULL,
  request_id TEXT NOT NULL,
  timestamp_ms INTEGER NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  endpoint TEXT NOT NULL,
  api_key_hash TEXT NOT NULL,
  input_tokens INTEGER NOT NULL,
  output_tokens INTEGER NOT NULL,
  latency_ms INTEGER NOT NULL,
  failed INTEGER NOT NULL,
  fail_status_code INTEGER,
  fail_summary TEXT
);

CREATE TABLE api_key_aliases (
  api_key_hash TEXT PRIMARY KEY,
  alias TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

CREATE TABLE model_prices (
  model TEXT PRIMARY KEY,
  prompt_per_1m REAL NOT NULL,
  completion_per_1m REAL NOT NULL,
  source TEXT NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

INSERT INTO usage_events VALUES
  ('fixture-event-initial-a', 'legacy-request-a', 200000000, 'openai', 'fixture-model', '/v1/chat/completions', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 11, 3, 120, 0, NULL, NULL),
  ('fixture-event-initial-b', 'legacy-request-b', 300000000, 'openai', 'fixture-model', '/v1/chat/completions', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 17, 5, 180, 1, 502, 'fixture upstream failure');

INSERT INTO api_key_aliases VALUES
  ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'Fixture Linux Codex', 300000000);

INSERT INTO model_prices VALUES
  ('fixture-model', 2.0, 4.0, 'fixture', 300000000);
