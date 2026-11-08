# Read-only pending settlement evidence plan

`memeloop-token-center plan-pending-settlement-recovery --manifest /private/manifest.json`
accepts one or two exact request identities:

```json
{
  "version": 1,
  "requests": [
    {
      "request_id": "00000000-0000-0000-0000-000000000001",
      "tenant_id": "00000000-0000-0000-0000-000000000002",
      "key_id": "00000000-0000-0000-0000-000000000003"
    }
  ]
}
```

Use the existing PostgreSQL and S3 configuration. The command opens one physical
database connection and a repeatable-read, read-only transaction. It performs
bounded exact-ID reads, rolls back and closes the connection before reading S3.
It does not initialize application state, migrate, run workers, write readiness
objects, contact a provider, create a receipt, or settle a reservation.
There is no apply option.

The plan verifies the original reservation contract digest and operational
digest, recovering upstream attribution only when a candidate exactly matches
the stored digest. The response must be bound to that request and reservation,
match its stored plaintext length, and contain a complete Responses JSON object
or exactly one complete `response.completed` SSE event. ArchiveStore handles its
normal codec; the existing canonical provider parser and settlement normalizer
preserve cached input, cache writes, service tier and coverage. Price estimates
use the original reservation price snapshot and the production pricing function.

Output contains identifiers, evidence digests, normalized usage, snapshot-priced
cost and missing-evidence codes. It excludes archive locators, response bodies,
owner tokens, credentials and raw SQL/storage errors. Unsupported, changed,
missing or invalid evidence produces a blocked row, never inferred zero usage.

Even a verified row is **not apply-ready**: the command cannot establish that the
old owner process has stopped, nor reconstruct all original financial routing
fields. Lease expiry alone does not prevent a live process from settling with
its existing fence. The database snapshot ends before archive reads; all
identities, contracts and financial states must be revalidated by a separately
reviewed apply workflow. Never copy a projected cost into the ledger manually.
