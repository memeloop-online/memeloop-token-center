-- A supplier-accepted reset remains locked until a later fresh credit read
-- proves that the old credit balance no longer applies. Unknown dispatch
-- results remain locked because a read cannot attribute their outcome.
ALTER TABLE upstream_quota_reset_operations
    ADD COLUMN settled_at BIGINT;

DROP INDEX upstream_quota_reset_one_active;
CREATE UNIQUE INDEX upstream_quota_reset_one_active
    ON upstream_quota_reset_operations (upstream_account_id)
    WHERE state IN ('prepared', 'submitted', 'accepted', 'unknown')
      AND settled_at IS NULL;
