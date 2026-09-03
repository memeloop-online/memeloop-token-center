CREATE TEMP TABLE key_credential_v60_guard (
    invalid BIGINT NOT NULL CHECK (invalid = 0)
);

-- Every imported credential must still point at a stable key. Active rows that
-- no longer describe the key's current generation would become silently
-- unusable after normalization, so fail the entire migration instead.
INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
LEFT JOIN key_records stable_key ON stable_key.id = legacy.key_id
WHERE stable_key.id IS NULL
   OR (legacy.revoked_at IS NULL AND legacy.generation <> stable_key.credential_generation)
LIMIT 1;

-- Preserve the exact SHA-256 source identity used for archive correlation.
-- This is generic credential provenance, not an authentication fallback.
-- Authentication uses only key_credentials after this migration.
INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials
WHERE source_hash !~ '^[0-9a-f]{64}$'
LIMIT 1;

INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials
WHERE fingerprint !~ '^[0-9a-f]{16}$'
   OR fingerprint <> SUBSTRING(ENCODE(secret_hash, 'hex') FROM 1 FOR 16)
LIMIT 1;

-- A normal credential is canonical when it already owns the stable key
-- generation. An active imported credential may share that slot only when it
-- has the same secret material. Choosing between two different active secrets
-- would silently revoke one of them, so make that operator-reconciliation work
-- explicit instead of guessing during a schema migration.
INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
JOIN key_credentials credential
  ON credential.key_id = legacy.key_id
 AND credential.generation = legacy.generation
WHERE legacy.revoked_at IS NULL
  AND (credential.revoked_at IS NOT NULL
       OR credential.secret_hash <> legacy.secret_hash
       OR credential.fingerprint <> legacy.fingerprint)
LIMIT 1;

INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
JOIN key_credentials credential ON credential.id = legacy.id
WHERE credential.key_id <> legacy.key_id
   OR credential.generation <> legacy.generation
   OR credential.secret_hash <> legacy.secret_hash
   OR credential.fingerprint <> legacy.fingerprint
LIMIT 1;

INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
JOIN key_credentials credential ON credential.secret_hash = legacy.secret_hash
WHERE credential.key_id <> legacy.key_id
   OR credential.generation <> legacy.generation
LIMIT 1;

INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM key_credentials
GROUP BY secret_hash
HAVING COUNT(*) > 1
LIMIT 1;

CREATE TABLE IF NOT EXISTS key_credential_source_proofs (
    credential_id TEXT NOT NULL REFERENCES key_credentials(id) ON DELETE CASCADE,
    proof_kind TEXT NOT NULL,
    source_digest TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (credential_id, proof_kind),
    UNIQUE (proof_kind, source_digest)
);

CREATE INDEX IF NOT EXISTS key_credential_source_proofs_digest_idx
    ON key_credential_source_proofs (source_digest, proof_kind, credential_id);

CREATE UNIQUE INDEX IF NOT EXISTS key_credentials_secret_hash_unique_idx
    ON key_credentials (secret_hash);

-- Rows that do not already have a normal sibling are copied verbatim. For a
-- safe coexistence the existing normal row remains canonical, retaining its
-- stable credential id and lifecycle metadata.
INSERT INTO key_credentials
    (id, key_id, generation, secret_hash, fingerprint, created_at, revoked_at)
SELECT legacy.id,
       legacy.key_id,
       legacy.generation,
       legacy.secret_hash,
       legacy.fingerprint,
       legacy.created_at,
       legacy.revoked_at
FROM legacy_key_credentials legacy
WHERE NOT EXISTS (
    SELECT 1
    FROM key_credentials credential
    WHERE credential.key_id = legacy.key_id
      AND credential.generation = legacy.generation
);

-- A stale partial application must never silently redirect a source digest to
-- a different credential. Matching proof rows are safe to replay.
INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
JOIN key_credentials credential
  ON credential.key_id = legacy.key_id
 AND credential.generation = legacy.generation
JOIN key_credential_source_proofs proof
  ON proof.proof_kind = 'external-source-key-hash-v1'
 AND (proof.source_digest = legacy.source_hash OR proof.credential_id = credential.id)
WHERE proof.source_digest <> legacy.source_hash
   OR proof.credential_id <> credential.id
   OR proof.created_at <> legacy.created_at
LIMIT 1;

INSERT INTO key_credential_source_proofs
    (credential_id, proof_kind, source_digest, created_at)
SELECT credential.id,
       'external-source-key-hash-v1',
       legacy.source_hash,
       legacy.created_at
FROM legacy_key_credentials legacy
JOIN key_credentials credential
  ON credential.key_id = legacy.key_id
 AND credential.generation = legacy.generation
ON CONFLICT (credential_id, proof_kind) DO NOTHING;

-- Verify the canonical credential and provenance sets before committing.
INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
LEFT JOIN key_credentials credential
  ON credential.key_id = legacy.key_id
 AND credential.generation = legacy.generation
WHERE credential.id IS NULL
   OR (legacy.revoked_at IS NULL
       AND (credential.revoked_at IS NOT NULL
            OR credential.secret_hash <> legacy.secret_hash
            OR credential.fingerprint <> legacy.fingerprint))
LIMIT 1;

INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
JOIN key_credentials credential
  ON credential.key_id = legacy.key_id
 AND credential.generation = legacy.generation
LEFT JOIN key_credential_source_proofs proof
  ON proof.credential_id = credential.id
 AND proof.proof_kind = 'external-source-key-hash-v1'
 AND proof.source_digest = legacy.source_hash
 AND proof.created_at = legacy.created_at
WHERE proof.credential_id IS NULL
LIMIT 1;

DROP TABLE legacy_key_credentials;
DROP TABLE key_credential_v60_guard;
