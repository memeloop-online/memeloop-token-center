-- Reader-first rollout: applying this migration does NOT activate the new
-- representation. Activation requires all observation readers (including
-- workers and external importers) to support context-prefix-v1 and old
-- processes to be drained. Never roll back readers after activation.
CREATE TABLE conversation_prefix_storage_policy (
    singleton BIGINT PRIMARY KEY CHECK (singleton = 1),
    reference_writes BIGINT NOT NULL CHECK (reference_writes IN (0, 1))
);
INSERT INTO conversation_prefix_storage_policy (singleton, reference_writes) VALUES (1, 0);
