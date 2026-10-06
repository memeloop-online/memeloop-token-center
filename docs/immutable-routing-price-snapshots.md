# Immutable routing and price snapshots (ledger 36)

This is the replacement for PR417, retaining master 602029fe and stacked on
PR469 at 36126f8a. PR469 is a hard dependency: merge 469 before 474, and never
deploy a 119-only release before 118. The integration includes 469 unchanged;
474 does not redesign its terminal projection protocol or change routing grants,
candidate selection, request concurrency, archive handoff or deployment.

## Sources of truth

- The public request model remains the authorized public model.
- Ordinary proxy admission persists the selected upstream model on the request
  in the same transaction as the existing usage reservation.
- Price lookup requires the actual selected upstream model and currency. A
  missing price fails closed; the public alias price is not a fallback.
- The existing reservation price snapshot contains the immutable rates and
  service/cache tiers used for billing. No second competing price snapshot is
  introduced.
- A pending candidate CAS updates its upstream model and reservation price
  together, even when token and money ceilings do not change. It preserves the
  reservation ID, RPM admission and concurrency allocation. Resizing the same
  assignment retains its admitted price rather than rereading a mutable tariff.
- Settlement/recovery already read the durable reservation snapshot; the new
  failover write fixes the stale-snapshot path without changing finalization.
- Request, event, conversation and session reads use the stored model, not a
  mutable route lookup. Historical rows remain unknown (NULL), not backfilled
  from today's route. Existing generation snapshots remain unchanged.

Migration 0119 adds only the nullable upstream-model column. Both backend
registries include 0118 from PR469 before 0119. Registry and upgrade assertions
require the 117,118,119 suffix; they must not be weakened for an isolated 119
head. No published migration or historical checksum is modified. Chart schema
metadata follows 119; this is not authorization to deploy. The production
candidate remains master 602029fe, excluding both PRs.

## Verification boundary

New SQLite/PostgreSQL cases cover same-amount/different-rate failover, stale CAS,
post-terminal rejection, tariff edits, preserved reservation identity, durable
recovery pricing and exactly-once settlement for prepaid and metered keys, with
terminal projection both disabled and enabled. Admission uses the real
gateway-persistence entrypoint; replay must retain the settled amount and update
the account exactly once, not reprice an old request from the current tariff. API
coverage distinguishes actual upstream pricing from a differently priced public
alias, preserves historical reads after route edits, and rejects missing actual
prices before dispatch. Existing route/conversation/event fixtures are retained.

Validation is GHA-only. No local build/test/install, staging verification,
production deployment, or migration execution is claimed by this document.
