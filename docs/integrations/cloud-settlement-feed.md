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
- This feed covers prepaid terminal snapshots published after migration 86.
  It does not bulk-backfill historical terminal traffic or include the separate
  metered-unlimited projection. Cloud should enable this on fresh prepaid
  accounts, or explicitly establish a cutover. A successful replay of an older
  terminal request can publish its snapshot at a new sequence.
- Deploy all gateway/control/worker binaries with migration 86 before enabling
  the Cloud consumer. An old writer does not publish feed rows; mixed versions
  are not a supported reconciliation cutover.

## Attributed rebate adjustments

Cloud may apply a post-settlement usage discount with
`PUT /internal/v1/accounts/{account_id}/settlements/{settlement_id}/adjustments`.
This is deliberately a separate, write-only capability: the caller needs
`settlements:adjust`, not `credits:write`. A tenant-scoped service token is
bound to the path account's tenant; an account outside that tenant is reported
as not found so the endpoint cannot become a tenant-discovery oracle.

The request names the original `request_kind` and `request_id`, its immutable
`currency`, a discount `namespace`, monotonically increasing `version`, and a
non-negative `desired_rebate`. The only accepted initial namespace is
`memeloop-cloud:usage-discount`; future namespaces need an explicit API
authorization change. It also includes an opaque `decision_digest` and
operator-safe `source`. `Idempotency-Key` is mandatory and scoped to
`(account_id, Idempotency-Key)` plus the canonical request: repeating the same
request returns the original event, while changing a request under an existing
key, supplying a stale version, or exceeding the original settled cost returns
409.

An adjustment never changes the gross settlement feed row, original usage
amount, or settlement identity. It can only move the attributed rebate forward
from zero up to that row's gross cost; an initial zero desired amount creates a
durable no-op decision event, but a value cannot subsequently be lowered. It
cannot make the usage charge negative.
The response reports desired, delta, cumulative and remaining money amounts,
plus opaque adjustment/event identifiers. A new reconciliation (including a
higher desired version) returns 201; an exact idempotent replay returns 200.
Responses are `Cache-Control: no-store` and expose no request payload,
credentials, reservation data, or raw idempotency value.

Discount reversal is intentionally not supported by this endpoint. A reversal
requires a separately versioned protocol, a distinct authorization capability,
and an explicit reference to the adjustment event being reversed; consumers
must not emulate it by lowering `desired_rebate`.

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
