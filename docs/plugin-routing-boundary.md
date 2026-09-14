# Plugin routing and availability boundary

## Implemented contract

The current `traffic-policy.post-auth` WIT hook can deny a request, rewrite its
model/body, and supply one account preference. Authentication precedes the
hook; both original and effective model require host route authorization.
The host database enumerates only active accounts on enabled, granted routes
in the tenant. The preference orders that set; it never creates a grant,
removes alternatives, or bypasses host health admission, transport validation,
budget reservation, or request auditing.

`order_authorized_candidates` accepts a mutable slice of that already-authorized
set and preserves every route/account/revision tuple. A matching preference
comes first, then route priority and weighted rendezvous order. A missing
preference leaves default ordering intact, including for an empty set. An
account on multiple granted routes retains each distinct route candidate.

The resolver emits a debug-level `authorized_candidate_order` event with
`tenant_id`, `key_id`, `selection_seed`, `candidate_count`, and a closed
`hint_disposition`: `absent`, `preferred`, or `outside_authorized_set`. It does
not log the supplied hint, model, request body, plugin reason, credentials,
configuration, or supplier response. `selection_seed` may be session-stable;
it is not necessarily the request ID. `preferred` proves ordering only, not
health, dispatch, or successful delivery. This diagnostic event supplements,
but does not replace, the request/attempt audit trail and failover metrics.

## Deliberately absent extensions

The current hook does not receive an authorized candidate snapshot, a typed
health/quota view, or a multi-account ranking result. Provider `prepare` and
`normalize` are bounded, buffered-only hooks, not arbitrary streaming data-flow
hooks. Operator UI contributions select core-owned data renderers; they do not
load arbitrary plugin JavaScript. Configuration cache freshness is not strict
cross-replica revocation. None of these limitations is fixed by a sorting log.

Safe future extensions need separate contracts and tests:

| Extension | Host-owned invariant |
| --- | --- |
| Candidate ranking/filtering | Pass opaque handles for the exact authorized set; validate unique subset/permutation results, cap size and execution, and revalidate eligibility at dispatch. Never accept a plugin-created account or route. Define filter-vs-preference explicitly. |
| Health/quota observation | Publish credential-free, generation-bound snapshots with source, observation time, expiry, and separate unknown/stale/exhausted states. A plugin observation is not permission to clear a breaker. |
| Recovery reconciliation | Require an independently authorized host operation, expected credential/health generation, fresh supplier evidence, idempotency and an audit receipt. An external reset or successful quota read alone must not reopen an account or spend a reset credit. |
| Request/response hooks | Pin one runtime contract for the request, cap buffering and execution, retain host SSRF/header/metering validation, and define a one-way dispatch/delivery boundary before adding streaming hooks. |
| Operator extension data | Use declared tenant-scoped, read-authorized feeds and core-owned renderers; expose freshness and partial failures without raw supplier payloads or secrets. |

Plugin failure must not silently skip required policy or fall back to an older
runtime. A plugin must never authorize replay: an HTTP 503 received after
dispatch is not proof that execution did not occur. Cooling an account and
authorizing a retry are separate decisions. Only host-proven pre-delivery
failure can advance safely; ambiguous delivery, visible output, and uncertain
non-idempotent actions remain terminal without replay.

## Runtime revision stack and integration order

The experimental stack is #55 (approved immutable inventory and lifecycle),
then #59 (bounded-stale configuration snapshots), then #68 (database-authoritative
application revision pinning). These drafts are not enabled by the production
executable and do not supply the absent availability hooks above.

For integration, first update #55 onto the current master and rerun its exact
CI; replay #59 onto that updated head; then replay #68, resolving application
entry-point and migration registry/chart/OpenAPI changes against current master.
Reallocate #68's proposed migration number if already owned; do not renumber a
published migration. Keep host inventory opt-in disabled until independent
review, exact CI and admission performance acceptance. The candidate-ordering
boundary here has no schema, WIT, or runtime-revision dependency and can merge
independently. Future ABI work must version its contract and extend approved
inventory compatibility checks rather than widening #68's fixed contract.

## Verification

Resolver unit contracts cover preferred-but-not-filtered selection, preservation
of revision tuples, ignored outside-set hints, empty sets, and distinct routes
for one account. Compilation and behavioral tests run in GitHub Actions; local
format/diff checks alone do not prove acceptance. No production state mutation,
supplier request, retry-policy change, or deployment is part of this slice.
