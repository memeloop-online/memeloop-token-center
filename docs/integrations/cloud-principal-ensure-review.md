# Cloud principal ensure: maintainer handoff

## Requested review boundary

The Cloud integration needs to provision a user before that user buys a plan,
so signup gifts and later subscriptions share one Token Center credit account.
The user authorized this upstream change and assigned the primary MTC maintenance
task to decide review, merge and rollout. This PR does not authorize its author
to merge or deploy MTC. Please leave questions and requested changes in PR comments.

Cloud changes remain in `memeloop-online/memeloop-cloud` PR #4. No MTC cluster
resources, credentials, database contents, or running images were changed here.

## API and invariants

`POST /internal/v1/integrations/memeloop-cloud/principals/ensure`

The JSON body contains only `tenant_external_id`, `principal_external_id` and
`currency`. Authentication requires a service Bearer with `keys:write`; a
tenant-scoped service token cannot ensure another tenant. Surrounding whitespace
is rejected to avoid disagreement between hashed identity and normalized database
identity. The existing control-router authentication/body bounds apply.

The response is HTTP 200 with the existing `ProvisionedCloudCredential` shape.
The endpoint reuses the exact framed-hash provisioning identity and transactional
helper used by Cloud subscription snapshots. It does not use generic key creation
as a lookup-then-create fallback.

- Initial account balance is zero; there is no grant or synthetic entitlement.
- Replays and concurrent calls retain the same account/key identity.
- Ensure before subscription and subscription before ensure use the same owner.
- Existing balance, policy and route grants must not change on ensure.
- Currency/binding conflicts fail; there is no implicit account replacement.
- The existing one-time credential replay window is unchanged. `key` can be null
  after expiry or rotation. Cloud must persist its encrypted copy; a missing copy
  requires explicit recovery, not hidden rotation or another account.

## Validation status at handoff

Passed locally:

- Rust 1.95 `cargo fmt --all -- --check`.
- OpenAPI route/role contract: 123 paths, 148 operations.
- Source module boundary test and operations TypeScript check.
- `git diff --check`.

Added SQLite HTTP test scenarios cover authentication/scope/tenant boundaries,
valid currency conflicts and invalid currency, replay, concurrent ensure, both
subscription orderings, zero initial funding and preservation of an existing
funded/routed account. **These Rust tests have not yet executed successfully.**
Local Cargo dependency acquisition encountered HTTP/2 errors and then repeated
timeouts even with multiplexing disabled; CI must prove compilation and execution.
The local dependency-install containers were stopped, and no test failure has
been relabeled as passing.

There is no new endpoint-specific PostgreSQL test yet. Existing PostgreSQL Cloud
subscription tests cover the reused provisioning helper, but are not a substitute
for the new service-Bearer route and its concurrent execution on PostgreSQL.
Please retain this gap in readiness review.

## Follow-up requirements outside this narrow PR

Cloud still needs to wire durable registration/ensure ordering, encrypted
principal storage, credit synchronization and provider credential resolution.
This PR alone does not make free trials or paid subscriptions production-ready.

Revocable/expiring gifts should use independent generic entitlement buckets and
explicit durable cancellation, not ordinary grant reversal after partial usage.
The current `period_end` does not independently remove pooled account balance.
Retail discount reconciliation is a separate contract; this endpoint neither
implements nor claims to solve it.

The existing operations dependency lock reported transitive `js-yaml` advisory
GHSA-2883-xcg3-v3hh during `npm audit`. This PR changes no dependency files;
please review that pre-existing tooling issue separately.

## Repository administration

The user explicitly authorized disabling `master-only-branches` (ruleset
22195408), which previously rejected feature branch creation and updates.
Only this ruleset's enforcement was changed; no other protection settings or
source visibility were changed. Future repository policy is the owner's decision.
