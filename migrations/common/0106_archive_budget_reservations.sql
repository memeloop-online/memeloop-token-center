-- Durable capacity ownership keeps the global counter out of request/session
-- transactions. The counter includes stored ciphertext and unused reservations.
-- Consuming a reservation atomically transfers capacity to an ordinary spool;
-- existing spool readers and garbage collectors continue to account actual bytes.
CREATE TABLE archive_budget_reservations (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN ('request', 'response')),
    cipher_bytes BIGINT NOT NULL CHECK (cipher_bytes BETWEEN 0 AND 268435456),
    expires_at BIGINT NOT NULL
);
CREATE INDEX archive_budget_reservations_expiry ON archive_budget_reservations(expires_at, id);
