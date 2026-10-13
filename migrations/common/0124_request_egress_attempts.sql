CREATE TABLE request_egress_attempts (
    request_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    outbound_attempt BIGINT NOT NULL CHECK (outbound_attempt > 0),
    connect_attempt BIGINT NOT NULL CHECK (connect_attempt > 0),
    route_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    transport_revision BIGINT NOT NULL,
    observed_at BIGINT NOT NULL,
    snapshot_json TEXT NOT NULL CHECK (LENGTH(snapshot_json) BETWEEN 2 AND 8192),
    transport_json TEXT NOT NULL CHECK (LENGTH(transport_json) BETWEEN 2 AND 8192),
    connection_binding_json TEXT CHECK (LENGTH(connection_binding_json) BETWEEN 2 AND 2048),
    request_payload_bytes BIGINT CHECK (request_payload_bytes >= 0),
    response_payload_bytes BIGINT CHECK (response_payload_bytes >= 0),
    completed BIGINT NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
    final_response BIGINT NOT NULL DEFAULT 0 CHECK (final_response IN (0, 1)),
    response_body_complete BIGINT NOT NULL DEFAULT 0 CHECK (response_body_complete IN (0, 1)),
    PRIMARY KEY (request_id, attempt_id)
);
CREATE INDEX request_egress_attempts_observed ON request_egress_attempts (observed_at, request_id);
