ALTER TABLE key_records ADD COLUMN provisioning_idempotency_key TEXT;
ALTER TABLE key_records ADD COLUMN provisioning_request_hash TEXT;
ALTER TABLE key_records ADD COLUMN issued_key_ciphertext TEXT;

CREATE UNIQUE INDEX key_records_provisioning_idempotency_idx
    ON key_records (provisioning_idempotency_key)
    WHERE provisioning_idempotency_key IS NOT NULL;
