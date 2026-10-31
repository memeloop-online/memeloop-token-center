-- 会话正文归档的媒体对象。用途是闭合的 `media`，写在对象键和
-- archive_staging_attempts 上，不写进密文字节。
-- 从原始请求 JSON 或响应抽出的媒体按 blake3 单独成对象。
-- CNPG part 保存该哈希和 locator。generation_assets 保持原路径，不合并。
-- 这是前向结构变更，不回填历史对象。
CREATE TABLE conversation_media_parts (
    tenant_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    part_index BIGINT NOT NULL CHECK (part_index >= 0),
    media_type TEXT NOT NULL CHECK (LENGTH(media_type) BETWEEN 1 AND 128),
    byte_length BIGINT NOT NULL CHECK (byte_length >= 0),
    content_blake3 TEXT NOT NULL CHECK (
        LENGTH(content_blake3) = 64
        AND content_blake3 = LOWER(content_blake3)
        AND content_blake3 ~ '^[0-9a-f]{64}$'
    ),
    object_locator TEXT NOT NULL CHECK (LENGTH(object_locator) BETWEEN 1 AND 1024),
    PRIMARY KEY (tenant_id, turn_id, part_index)
);

CREATE INDEX conversation_media_parts_blake3_idx
    ON conversation_media_parts (content_blake3);

CREATE INDEX conversation_media_parts_locator_idx
    ON conversation_media_parts (object_locator);

-- Replace the closed purpose checks. Constraint names are assigned by PostgreSQL,
-- so match them by definition instead of guessing a suffix.
DO $$
DECLARE
    constraint_name text;
BEGIN
    FOR constraint_name IN
        SELECT conname
        FROM pg_constraint
        WHERE conrelid = 'archive_staging_attempts'::regclass
          AND contype = 'c'
          AND pg_get_constraintdef(oid) LIKE '%purpose%'
          AND pg_get_constraintdef(oid) LIKE '%request%'
    LOOP
        EXECUTE format(
            'ALTER TABLE archive_staging_attempts DROP CONSTRAINT %I',
            constraint_name
        );
    END LOOP;
END $$;

ALTER TABLE archive_staging_attempts
    ADD CONSTRAINT archive_staging_attempts_purpose_check
    CHECK (purpose IN ('request', 'response', 'result', 'assets', 'media'));

ALTER TABLE archive_staging_attempts
    ADD CONSTRAINT archive_staging_attempts_owner_purpose_check
    CHECK (
        (owner_kind = 'proxy_request' AND purpose IN ('request', 'response', 'media'))
        OR (owner_kind = 'synchronous_request' AND purpose IN ('request', 'result', 'media'))
        OR (owner_kind = 'generation_job' AND purpose IN ('request', 'assets'))
    );
