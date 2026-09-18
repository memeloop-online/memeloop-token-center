CREATE TABLE upstream_connection_failure_domains (
    upstream_account_id TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    transport_revision BIGINT NOT NULL,
    failure_domain TEXT NOT NULL,
    last_failure_epoch TEXT NOT NULL,
    last_request_id TEXT NOT NULL,
    failure_stage TEXT NOT NULL,
    gateway_pod TEXT NOT NULL,
    gateway_node TEXT,
    first_failure_at BIGINT NOT NULL,
    last_failure_at BIGINT NOT NULL,
    PRIMARY KEY (
        upstream_account_id,
        credential_generation,
        transport_revision,
        failure_domain
    )
);

CREATE INDEX idx_upstream_connection_failure_domains_window
    ON upstream_connection_failure_domains (
        upstream_account_id,
        credential_generation,
        transport_revision,
        last_failure_at
    );

CREATE TABLE request_upstream_transport_diagnostics (
    request_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    transport_revision BIGINT NOT NULL,
    failure_kind TEXT NOT NULL,
    failure_stage TEXT NOT NULL,
    gateway_pod TEXT NOT NULL,
    gateway_node TEXT,
    failure_domain TEXT NOT NULL,
    observed_at BIGINT NOT NULL,
    PRIMARY KEY (request_id, route_id, upstream_account_id)
);

CREATE INDEX idx_request_upstream_transport_diagnostics_request
    ON request_upstream_transport_diagnostics (request_id, observed_at);
