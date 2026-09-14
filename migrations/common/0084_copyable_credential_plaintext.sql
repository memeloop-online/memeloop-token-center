-- Operators explicitly requested durable plaintext for credentials that may be
-- copied later.  Keep it nullable: hash-only historical credentials are not
-- reconstructable and must never be replaced or guessed.
ALTER TABLE key_credentials ADD COLUMN secret_plaintext TEXT;
ALTER TABLE service_credentials ADD COLUMN secret_plaintext TEXT;
