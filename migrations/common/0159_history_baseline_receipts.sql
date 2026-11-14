-- Receipts are metadata, never a second history payload store. A PostgreSQL
-- snapshot cannot be resumed after losing its connection. Aborted runs remain
-- explicit evidence; their cursor must never be reused as a consistent epoch.
CREATE TABLE request_history_baseline_runs (
    owner TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    ceiling BIGINT NOT NULL,
    source_snapshot TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active','source_complete','aborted')),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (owner,epoch)
);
CREATE TABLE request_history_baseline_chunks (
    owner TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    ordinal BIGINT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('native','generation','archive','event')),
    after_id TEXT NOT NULL,
    through_id TEXT NOT NULL,
    row_count BIGINT NOT NULL CHECK (row_count BETWEEN 1 AND 64),
    source_hash TEXT NOT NULL,
    target_hash TEXT NOT NULL,
    PRIMARY KEY (owner,epoch,ordinal),
    FOREIGN KEY (owner,epoch) REFERENCES request_history_baseline_runs (owner,epoch),
    CHECK (source_hash=target_hash)
);
