-- A durable maintenance fence, not a request admission limit or retention rule.
CREATE TABLE request_history_delivery_control (
    id BIGINT PRIMARY KEY CHECK (id=1),
    epoch BIGINT NOT NULL DEFAULT 0,
    owner TEXT,
    ceiling BIGINT,
    CHECK ((owner IS NULL AND ceiling IS NULL) OR (owner IS NOT NULL AND ceiling IS NOT NULL))
);
INSERT INTO request_history_delivery_control (id) VALUES (1);
