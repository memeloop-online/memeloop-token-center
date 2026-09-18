-- Application migration 110 first verifies and promotes every recoverable
-- active NULL plaintext inside the same transaction. Never run this SQL alone.
DROP TABLE key_credential_recovery_access_audit;
DROP TABLE key_credential_recovery_rate_limits;
DROP TABLE key_credential_recovery_audit;
DROP TABLE key_credential_recovery_secrets;
