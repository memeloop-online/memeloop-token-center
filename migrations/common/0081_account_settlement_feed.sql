-- Settlement visibility is assigned only after both the usage ledger entry and
-- the terminal request snapshot exist in the same transaction. The counter is
-- account-local so concurrent commits for one account have a stable order.
ALTER TABLE credit_accounts
    ADD COLUMN settlement_sequence BIGINT NOT NULL DEFAULT 0;

CREATE TABLE account_settlement_feed (
    settlement_id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    settlement_sequence BIGINT NOT NULL CHECK (settlement_sequence > 0),
    request_id TEXT NOT NULL,
    request_kind TEXT NOT NULL CHECK (request_kind IN ('text', 'generation')),
    key_id TEXT NOT NULL,
    model TEXT NOT NULL,
    cost_micros BIGINT NOT NULL CHECK (cost_micros >= 0),
    currency TEXT NOT NULL,
    settled_at BIGINT NOT NULL,
    completed_at BIGINT NOT NULL,
    input_tokens BIGINT,
    cached_input_tokens BIGINT,
    cache_write_tokens BIGINT,
    output_tokens BIGINT,
    UNIQUE(account_id, settlement_sequence),
    UNIQUE(request_kind, request_id)
);

CREATE INDEX account_settlement_feed_account_request_idx
    ON account_settlement_feed (account_id, request_id);

CREATE INDEX ledger_entries_usage_reservation_idx
    ON ledger_entries (account_id, key_id, source)
    WHERE kind = 'usage';
