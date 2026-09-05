ALTER TABLE upstream_account_health
    ADD COLUMN probe_lease_token TEXT NOT NULL DEFAULT '';

ALTER TABLE upstream_account_health
    ADD COLUMN credential_generation BIGINT NOT NULL DEFAULT 0;

-- Health rows predate credential-generation fencing. Preserve their cooldown
-- and probe-lease state by attaching them to the account generation that was
-- current at the v65 -> v66 upgrade boundary.
UPDATE upstream_account_health
SET credential_generation = (
    SELECT account.credential_generation
    FROM upstream_accounts account
    WHERE account.id = upstream_account_health.upstream_account_id
)
WHERE EXISTS (
    SELECT 1
    FROM upstream_accounts account
    WHERE account.id = upstream_account_health.upstream_account_id
);
