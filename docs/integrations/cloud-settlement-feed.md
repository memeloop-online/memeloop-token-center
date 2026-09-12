# Account settlement evidence for Cloud reconciliation

Cloud must reconcile its customer ledger against the exact cost MTC charged.
Recomputing from a separately fetched model catalog can disagree with MTC's
admitted price snapshot, rounding or balance limits. The new account settlement
endpoint provides the actual usage-ledger identity and amount bound to the
request, account, key, model and currency.

## Contract and rollout

- `GET /internal/v1/accounts/{account_id}/settlements` requires both
  `credits:read` and `requests:read`. Tenant-scoped tokens cannot cross accounts
  outside their tenant. Issue a dedicated token for this use.
- `request_kind` (`text` or `generation`) and `request_id` together perform an
  exact lookup. Both fields are required together and cannot be mixed with a
  pagination cursor. An empty result means no published
  settlement, never an inferred zero charge.
- A feed page contains `items` and nullable `next_cursor`. Ascending cursors use
  `after_sequence` with `after_id`, both taken from an existing account row.
  Pollers retain the last item's cursor even when `next_cursor` is null.
- The response contains no prompt, response body, archive locator, reservation
  ID, upstream credential or customer contact information. Responses are
  `Cache-Control: no-store`.
- This feed covers prepaid terminal snapshots published after migration 81.
  It does not bulk-backfill historical terminal traffic or include the separate
  metered-unlimited projection. Cloud should enable this on fresh prepaid
  accounts, or explicitly establish a cutover. A successful replay of an older
  terminal request can publish its snapshot at a new sequence.
- Deploy all gateway/control/worker binaries with migration 81 before enabling
  the Cloud consumer. An old writer does not publish feed rows; mixed versions
  are not a supported reconciliation cutover.

## Transaction invariants

Each terminal transaction reads the actual usage ledger and rejects an amount
that disagrees with its terminal record. While holding the account row lock, it
increments an account-local counter and inserts an immutable snapshot. Rollback
reverts both. The account row lock serializes publishers across database
connections and application instances, so sequence order follows commit order.

Sequence allocation happens only after terminal details exist. A split
settle/finish request therefore cannot disappear behind an already-consumed
cursor. Replays keep the original settlement identity and sequence. Generation
completion, queued cancellation and preparation failure share one terminal
effects entry point; zero-cost terminal results still publish evidence.

The migration adds an account sequence column, snapshot table and a partial
usage-reservation lookup index. The snapshot survives request-history retention.
The index build is part of the migration transaction and should be scheduled
appropriately for an existing large ledger.
