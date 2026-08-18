-- A migrated legacy credential and a stable downstream key are globally one-to-one.
-- Creating the unique index deliberately fails closed when pre-existing duplicate
-- targets are present, so operators must reconcile those mappings before migration.
CREATE UNIQUE INDEX legacy_key_credentials_key_unique_idx
    ON legacy_key_credentials (key_id);
