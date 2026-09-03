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

-- Preserve the exact SHA-256 source identity used by CPAMP and archive
-- correlation. This is generic credential provenance, not an authentication
-- fallback. Authentication uses only key_credentials after this migration.
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

-- The normal credential model is one secret per stable key generation. Do not
-- weaken that invariant or choose one of two secrets implicitly.
INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
JOIN key_credentials credential
  ON credential.key_id = legacy.key_id
 AND credential.generation = legacy.generation
LIMIT 1;

INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
JOIN key_credentials credential ON credential.id = legacy.id
LIMIT 1;

INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
JOIN key_credentials credential ON credential.secret_hash = legacy.secret_hash
LIMIT 1;

INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM key_credentials
GROUP BY secret_hash
HAVING COUNT(*) > 1
LIMIT 1;

CREATE TABLE IF NOT EXISTS key_credential_source_proofs (
    credential_id TEXT NOT NULL,
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

INSERT INTO key_credentials
    (id, key_id, generation, secret_hash, fingerprint, created_at, revoked_at)
SELECT id, key_id, generation, secret_hash, fingerprint, created_at, revoked_at
FROM legacy_key_credentials;

INSERT INTO key_credential_source_proofs
    (credential_id, proof_kind, source_digest, created_at)
SELECT id, 'legacy-source-hash-v1', source_hash, created_at
FROM legacy_key_credentials;

-- Verify the exact copied credential and provenance sets before committing.
INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
LEFT JOIN key_credentials credential
  ON credential.id = legacy.id
 AND credential.key_id = legacy.key_id
 AND credential.generation = legacy.generation
 AND credential.secret_hash = legacy.secret_hash
 AND credential.fingerprint = legacy.fingerprint
 AND credential.created_at = legacy.created_at
 AND COALESCE(credential.revoked_at, -1) = COALESCE(legacy.revoked_at, -1)
WHERE credential.id IS NULL
LIMIT 1;

INSERT INTO key_credential_v60_guard (invalid)
SELECT 1
FROM legacy_key_credentials legacy
LEFT JOIN key_credential_source_proofs proof
  ON proof.credential_id = legacy.id
 AND proof.proof_kind = 'legacy-source-hash-v1'
 AND proof.source_digest = legacy.source_hash
 AND proof.created_at = legacy.created_at
WHERE proof.credential_id IS NULL
LIMIT 1;

DROP TABLE key_credential_v60_guard;
