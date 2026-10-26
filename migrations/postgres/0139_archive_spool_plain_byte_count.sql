-- 64MiB 曾经是单份明文上限。原始请求 JSON 和原始返回 JSON 现在整份归档，
-- 不再按这个数截断。分片数上限仍是 65536，每片 64KiB，所以明文 byte_count
-- 上限改为 4GiB。密文预算仍由 response_archive_spool_budget 限制。

DO $$
DECLARE
    constraint_name text;
BEGIN
    FOR constraint_name IN
        SELECT conname
        FROM pg_constraint
        WHERE conrelid = 'response_archive_spools'::regclass
          AND contype = 'c'
          AND pg_get_constraintdef(oid) LIKE '%67108864%'
    LOOP
        EXECUTE format(
            'ALTER TABLE response_archive_spools DROP CONSTRAINT %I',
            constraint_name
        );
    END LOOP;
    FOR constraint_name IN
        SELECT conname
        FROM pg_constraint
        WHERE conrelid = 'request_archive_spools'::regclass
          AND contype = 'c'
          AND pg_get_constraintdef(oid) LIKE '%67108864%'
    LOOP
        EXECUTE format(
            'ALTER TABLE request_archive_spools DROP CONSTRAINT %I',
            constraint_name
        );
    END LOOP;
END $$;

ALTER TABLE response_archive_spools
    ADD CONSTRAINT response_archive_spools_byte_count_check
    CHECK (byte_count BETWEEN 0 AND 4294967296);

ALTER TABLE request_archive_spools
    ADD CONSTRAINT request_archive_spools_byte_count_check
    CHECK (byte_count BETWEEN 0 AND 4294967296);
