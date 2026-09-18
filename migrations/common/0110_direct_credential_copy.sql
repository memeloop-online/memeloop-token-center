-- Expand phase: application migration 110 validates every active credential
-- and promotes missing plaintext inside this same transaction. The application
-- no longer accesses legacy envelope/access tables, but they MUST remain while
-- old control replicas can still serve requests during a rolling upgrade.
-- A separate future contract migration may remove them only after production
-- rollout verification. Never run this SQL without the application preflight.
SELECT 1;
