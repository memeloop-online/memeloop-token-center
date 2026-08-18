-- Durable ownership and fenced cleanup for object-store staging prefixes.
--
-- Object paths are deliberately not stored here. The application reconstructs
-- the only deletable prefix from the typed owner, purpose, and attempt UUIDs.
-- This prevents a corrupted row from turning the cleanup worker into an
-- arbitrary-prefix deletion primitive.
CREATE TABLE archive_staging_attempts (
    attempt_id TEXT PRIMARY KEY,
    owner_kind TEXT NOT NULL CHECK (
        owner_kind IN ('proxy_request', 'synchronous_request', 'generation_job')
    ),
    owner_id TEXT NOT NULL,
    purpose TEXT NOT NULL CHECK (
        purpose IN ('request', 'response', 'result', 'assets')
    ),
    intent_digest TEXT NOT NULL,
    state TEXT NOT NULL CHECK (
        state IN ('writing', 'bound', 'cleanup_pending', 'cleaned')
    ),
    writer_owner TEXT NOT NULL,
    writer_token TEXT NOT NULL,
    lease_owner TEXT,
    lease_token TEXT,
    lease_expires_at BIGINT,
    bound_locator TEXT,
    bound_at BIGINT,
    empty_observed_at BIGINT,
    cleanup_failures BIGINT NOT NULL DEFAULT 0 CHECK (
        cleanup_failures BETWEEN 0 AND 63
    ),
    next_cleanup_at BIGINT,
    last_error_code TEXT CHECK (
        last_error_code IS NULL OR last_error_code IN (
            'object_store_unavailable',
            'delete_failed',
            'verification_failed',
            'reference_check_failed',
            'reference_present'
        )
    ),
    created_at BIGINT NOT NULL CHECK (created_at >= 0),
    updated_at BIGINT NOT NULL CHECK (updated_at >= created_at),
    cleaned_at BIGINT,

    CHECK (
        (owner_kind = 'proxy_request' AND purpose IN ('request', 'response'))
        OR (owner_kind = 'synchronous_request' AND purpose IN ('request', 'result'))
        OR (owner_kind = 'generation_job' AND purpose IN ('request', 'assets'))
    ),
    CHECK (LENGTH(writer_owner) BETWEEN 1 AND 128),
    CHECK (LENGTH(lease_owner) BETWEEN 1 AND 128 OR lease_owner IS NULL),
    CHECK (lease_expires_at IS NULL OR lease_expires_at >= 0),
    CHECK (LENGTH(bound_locator) BETWEEN 1 AND 1024 OR bound_locator IS NULL),
    CHECK (bound_at IS NULL OR bound_at >= created_at),
    CHECK (empty_observed_at IS NULL OR empty_observed_at >= created_at),
    CHECK (next_cleanup_at IS NULL OR next_cleanup_at >= created_at),
    CHECK (cleaned_at IS NULL OR cleaned_at >= created_at),

    -- Portable lower-case UUID validation for PostgreSQL and SQLite.
    CHECK (
        LENGTH(attempt_id) = 36
        AND SUBSTR(attempt_id, 9, 1) = '-'
        AND SUBSTR(attempt_id, 14, 1) = '-'
        AND SUBSTR(attempt_id, 19, 1) = '-'
        AND SUBSTR(attempt_id, 24, 1) = '-'
        AND LOWER(attempt_id) = attempt_id
        AND LENGTH(
            REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
            REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
            REPLACE(attempt_id, '-', ''), '0', ''), '1', ''), '2', ''), '3', ''),
            '4', ''), '5', ''), '6', ''), '7', ''), '8', ''), '9', ''), 'a', ''),
            'b', ''), 'c', ''), 'd', ''), 'e', ''), 'f', '')
        ) = 0
    ),
    CHECK (
        LENGTH(owner_id) = 36
        AND SUBSTR(owner_id, 9, 1) = '-'
        AND SUBSTR(owner_id, 14, 1) = '-'
        AND SUBSTR(owner_id, 19, 1) = '-'
        AND SUBSTR(owner_id, 24, 1) = '-'
        AND LOWER(owner_id) = owner_id
        AND LENGTH(
            REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
            REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
            REPLACE(owner_id, '-', ''), '0', ''), '1', ''), '2', ''), '3', ''),
            '4', ''), '5', ''), '6', ''), '7', ''), '8', ''), '9', ''), 'a', ''),
            'b', ''), 'c', ''), 'd', ''), 'e', ''), 'f', '')
        ) = 0
    ),
    CHECK (
        LENGTH(intent_digest) = 64
        AND LOWER(intent_digest) = intent_digest
        AND LENGTH(
            REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
            REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
            intent_digest, '0', ''), '1', ''), '2', ''), '3', ''), '4', ''),
            '5', ''), '6', ''), '7', ''), '8', ''), '9', ''), 'a', ''), 'b', ''),
            'c', ''), 'd', ''), 'e', ''), 'f', '')
        ) = 0
    ),
    CHECK (
        LENGTH(writer_token) = 36
        AND SUBSTR(writer_token, 9, 1) = '-'
        AND SUBSTR(writer_token, 14, 1) = '-'
        AND SUBSTR(writer_token, 19, 1) = '-'
        AND SUBSTR(writer_token, 24, 1) = '-'
        AND LOWER(writer_token) = writer_token
        AND LENGTH(
            REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
            REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
            REPLACE(writer_token, '-', ''), '0', ''), '1', ''), '2', ''), '3', ''),
            '4', ''), '5', ''), '6', ''), '7', ''), '8', ''), '9', ''), 'a', ''),
            'b', ''), 'c', ''), 'd', ''), 'e', ''), 'f', '')
        ) = 0
    ),
    CHECK (
        lease_token IS NULL OR (
            LENGTH(lease_token) = 36
            AND SUBSTR(lease_token, 9, 1) = '-'
            AND SUBSTR(lease_token, 14, 1) = '-'
            AND SUBSTR(lease_token, 19, 1) = '-'
            AND SUBSTR(lease_token, 24, 1) = '-'
            AND LOWER(lease_token) = lease_token
            AND LENGTH(
                REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
                REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(REPLACE(
                REPLACE(lease_token, '-', ''), '0', ''), '1', ''), '2', ''), '3', ''),
                '4', ''), '5', ''), '6', ''), '7', ''), '8', ''), '9', ''), 'a', ''),
                'b', ''), 'c', ''), 'd', ''), 'e', ''), 'f', '')
            ) = 0
        )
    ),

    -- Every state has one canonical nullable-field shape. In particular, a
    -- cleanup lease is all-or-nothing and a bound attempt can never be claimed.
    CHECK (
        (
            state = 'writing'
            AND lease_owner IS NOT NULL
            AND lease_token IS NOT NULL
            AND lease_expires_at IS NOT NULL
            AND lease_owner = writer_owner
            AND lease_token = writer_token
            AND bound_locator IS NULL
            AND bound_at IS NULL
            AND empty_observed_at IS NULL
            AND next_cleanup_at IS NULL
            AND cleaned_at IS NULL
        ) OR (
            state = 'bound'
            AND lease_owner IS NULL
            AND lease_token IS NULL
            AND lease_expires_at IS NULL
            AND bound_locator IS NOT NULL
            AND bound_at IS NOT NULL
            AND empty_observed_at IS NULL
            AND next_cleanup_at IS NULL
            AND cleaned_at IS NULL
        ) OR (
            state = 'cleanup_pending'
            AND bound_locator IS NULL
            AND cleaned_at IS NULL
            AND next_cleanup_at IS NOT NULL
            AND (
                (
                    lease_owner IS NULL
                    AND lease_token IS NULL
                    AND lease_expires_at IS NULL
                ) OR (
                    lease_owner IS NOT NULL
                    AND lease_token IS NOT NULL
                    AND lease_expires_at IS NOT NULL
                )
            )
        ) OR (
            state = 'cleaned'
            AND lease_owner IS NULL
            AND lease_token IS NULL
            AND lease_expires_at IS NULL
            AND bound_locator IS NULL
            AND empty_observed_at IS NOT NULL
            AND next_cleanup_at IS NULL
            AND cleaned_at IS NOT NULL
        )
    )
);

CREATE INDEX archive_staging_cleanup_claim_idx
    ON archive_staging_attempts (
        state,
        next_cleanup_at,
        lease_expires_at,
        attempt_id
    );

CREATE INDEX archive_staging_stale_writing_idx
    ON archive_staging_attempts (state, lease_expires_at, attempt_id);

CREATE INDEX archive_staging_owner_idx
    ON archive_staging_attempts (owner_kind, owner_id, purpose, created_at);
