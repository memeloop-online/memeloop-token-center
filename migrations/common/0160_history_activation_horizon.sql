-- Capture activation metadata only. No request/history payload is duplicated.
CREATE TABLE request_history_activation (
 id BIGINT PRIMARY KEY CHECK (id=1),
 epoch BIGINT NOT NULL DEFAULT 0,
 phase TEXT NOT NULL DEFAULT 'legacy' CHECK (phase IN ('legacy','pending','observing','drained')),
 transactions_json TEXT,
 baseline_owner TEXT,
 baseline_epoch BIGINT
);
INSERT INTO request_history_activation (id) VALUES (1);
