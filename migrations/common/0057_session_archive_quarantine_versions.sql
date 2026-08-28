-- Quarantine evidence is append-only across corrected archive records and
-- source-session moves.  The v1 table remains intact as compatibility/audit
-- evidence.  V2 readers use immutable versions plus a mutable head pointer.
CREATE TABLE session_archive_quarantine_record_versions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    cpamp_source TEXT NOT NULL,
    external_request_id TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    record_digest TEXT NOT NULL,
    identity_claim_digest TEXT,
    reason_code TEXT NOT NULL CHECK (
        reason_code IN ('missing_credential_hash', 'unproven_identity')
    ),
    proof_digest TEXT NOT NULL,
    source_started_at BIGINT NOT NULL,
    source_completed_at BIGINT,
    protocol TEXT NOT NULL,
    model TEXT NOT NULL,
    status_code BIGINT,
    duration_ms BIGINT,
    input_tokens BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    error_code TEXT,
    request_digest TEXT,
    response_digest TEXT,
    request_object TEXT,
    response_object TEXT,
    quarantined_at BIGINT NOT NULL,
    UNIQUE (
        tenant_id,
        source,
        external_request_id,
        record_digest,
        source_session_id
    )
);

CREATE INDEX session_archive_quarantine_version_page_idx
    ON session_archive_quarantine_record_versions
       (tenant_id, source_started_at DESC, id DESC);
CREATE INDEX session_archive_quarantine_version_request_idx
    ON session_archive_quarantine_record_versions
       (tenant_id, source, external_request_id, quarantined_at DESC, id DESC);
CREATE INDEX session_archive_quarantine_version_session_idx
    ON session_archive_quarantine_record_versions
       (tenant_id, source, source_session_id, external_request_id);

-- This is the only mutable quarantine relation.  Repointing the head never
-- updates or deletes evidence, batch occurrences, or operator resolutions.
CREATE TABLE session_archive_quarantine_record_heads (
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    external_request_id TEXT NOT NULL,
    quarantine_id TEXT NOT NULL,
    record_digest TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, source, external_request_id),
    UNIQUE (tenant_id, source, quarantine_id)
);

-- Each sealed batch records the evidence version and source session observed
-- in that batch.  Exact replay addresses the same (batch, sequence) row.
-- overlapping batches may independently retain the same immutable version.
CREATE TABLE session_archive_quarantine_occurrences (
    tenant_id TEXT NOT NULL,
    batch_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    quarantine_id TEXT NOT NULL,
    record_digest TEXT NOT NULL,
    source_session_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (batch_id, sequence),
    UNIQUE (batch_id, quarantine_id)
);

CREATE INDEX session_archive_quarantine_occurrence_version_idx
    ON session_archive_quarantine_occurrences (quarantine_id, batch_id);
CREATE INDEX session_archive_quarantine_occurrence_session_idx
    ON session_archive_quarantine_occurrences
       (tenant_id, source_session_id, batch_id, sequence);

-- Preserve every v1 evidence identity and resolution target exactly.  A legacy
-- blank source_session_id is deliberately retained until a same-record replay
-- supplies the sealed session identity as a new immutable version.
INSERT INTO session_archive_quarantine_record_versions (
    id, tenant_id, source, cpamp_source, external_request_id,
    source_session_id, record_digest, identity_claim_digest, reason_code,
    proof_digest, source_started_at, source_completed_at, protocol, model,
    status_code, duration_ms, input_tokens, output_tokens, error_code,
    request_digest, response_digest, request_object, response_object,
    quarantined_at
)
SELECT
    id, tenant_id, source, cpamp_source, external_request_id,
    source_session_id, record_digest, identity_claim_digest, reason_code,
    proof_digest, source_started_at, source_completed_at, protocol, model,
    status_code, duration_ms, input_tokens, output_tokens, error_code,
    request_digest, response_digest, request_object, response_object,
    quarantined_at
FROM session_archive_quarantine_records;

INSERT INTO session_archive_quarantine_record_heads (
    tenant_id, source, external_request_id, quarantine_id, record_digest,
    source_session_id, updated_at
)
SELECT
    tenant_id, source, external_request_id, id, record_digest,
    source_session_id, quarantined_at
FROM session_archive_quarantine_records;

INSERT INTO session_archive_quarantine_occurrences (
    tenant_id, batch_id, sequence, quarantine_id, record_digest,
    source_session_id, created_at
)
SELECT
    membership.tenant_id,
    membership.batch_id,
    membership.sequence,
    membership.quarantine_id,
    record.record_digest,
    record.source_session_id,
    membership.created_at
FROM session_archive_quarantine_batch_records AS membership
JOIN session_archive_quarantine_records AS record
  ON record.id = membership.quarantine_id;
