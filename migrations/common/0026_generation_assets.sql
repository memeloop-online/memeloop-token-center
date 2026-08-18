CREATE TABLE generation_assets (
    id TEXT PRIMARY KEY,
    job_id TEXT REFERENCES generation_jobs(id) ON DELETE CASCADE,
    request_id TEXT REFERENCES request_record_locators(id) ON DELETE CASCADE,
    asset_index BIGINT NOT NULL CHECK (asset_index >= 0),
    object_locator TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    size_bytes BIGINT NOT NULL CHECK (size_bytes >= 0),
    filename TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    CHECK ((job_id IS NOT NULL) <> (request_id IS NOT NULL)),
    UNIQUE (job_id, asset_index),
    UNIQUE (request_id, asset_index)
);

CREATE INDEX generation_assets_job_idx
    ON generation_assets (job_id, asset_index, id);

CREATE INDEX generation_assets_request_idx
    ON generation_assets (request_id, asset_index, id);

CREATE TABLE synchronous_image_idempotency (
    key_id TEXT NOT NULL REFERENCES key_records(id) ON DELETE CASCADE,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    request_id TEXT NOT NULL,
    reservation_id TEXT REFERENCES usage_reservations(id),
    status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'failed')),
    response_status BIGINT,
    response_object TEXT,
    error_code TEXT,
    created_at BIGINT NOT NULL,
    lease_expires_at BIGINT NOT NULL,
    completed_at BIGINT,
    PRIMARY KEY (key_id, idempotency_key)
);

CREATE INDEX synchronous_image_idempotency_request_idx
    ON synchronous_image_idempotency (request_id);

CREATE UNIQUE INDEX synchronous_image_idempotency_reservation_idx
    ON synchronous_image_idempotency (reservation_id)
    WHERE reservation_id IS NOT NULL;
