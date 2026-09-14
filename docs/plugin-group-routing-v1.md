# Group routing plugin ABI v1

## Operator and host behavior

Provider and route group editors expose a native/installed strategy selector,
the plugin's schema form, and an explicit integer priority. Credential groups
cannot select a strategy. PUT `/internal/v1/provider-groups/{id}/routing-strategy`
and the corresponding route-group endpoint require tenant scope, the current
`expected_updated_at`, and `expected_strategy_version`; null restores native.
Conflicts refresh the version while preserving the operator's draft for an
explicit retry. Version numbers increase even when clearing a strategy.

For each already-authorized candidate, eligible provider groups must both
contain its account and be explicitly included by its route. Route group
membership can select a strategy but never authorizes that route. Highest
priority wins; equal priorities use group UUID ascending, then group kind.
Configured buckets run in that order before unconfigured native candidates.
The selected plan must preserve every candidate. Sticky candidates form a
session-seeded rendezvous tier; non-sticky candidates retain plugin order.
Tenant and credential-key identity are already mixed into the session seed.

The native primary fixes the original attempt/recovery budget before any
strategy reordering. `remaining_deadline_ms` is that remaining core recovery
budget, not a renewable plugin timeout. Plan cannot extend it. The core clamps
transient cooldown overrides to 60 seconds, rechecks to 25–5000 ms, retains its
bounded waiter count, and uses the existing cross-Pod exclusive probe lease.
Hard quota/rate-limit/authentication evidence is never shortened by a strategy.
Terminal observe feeds bounded transient cooldown into the existing fenced
health update; observe never initiates waiting, probing, or request replay.

With no installed routing hooks, the native entrance performs no added database
query or candidate cloning; any stale binding remains inert native fallback.
With installed hooks but no configured group, the native path performs no hook
calls or candidate policy queries beyond the bounded tenant existence check.
Invalid/missing/trapping strategy code records
`group_routing_native_fallback` and retains native handling of that bucket's
authorized candidates. `group_routing_observe_fallback` retains native terminal
health handling. No configuration, credentials, payloads or plugin error text
are logged. One database statement pins every candidate's applicable group,
priority, membership, nullable configuration and version together; a running
request never switches to newer config or plugin runtime.

Text proxy requests use this contract for both direct and buffered component
providers. Component providers always pass the core health admission gate,
including when planning fails or no strategy is selected: fallback cannot
revive hard quota/authentication evidence. Invalid received responses complete
as failed attempts without replay. The component normalization adapter currently
classifies guest traps and malformed output alike as response-processing
failures; local capacity/storage failures remain inconclusive.
This also fixes a pre-existing component-path bug: that path previously skipped
core health admission entirely. Native selection and ordering remain unchanged,
but an unconfigured component account with known hard isolation evidence is now
correctly blocked by the existing native health policy, just like direct providers.

Asynchronous media jobs and the media synchronous-entry path are not wired to
this contract yet; their durable strategy/revision pinning is separate pending
work, not covered by this text-proxy implementation.

## ABI

This scheduling contract is independent of `traffic-policy.post-auth`. Build
the `group-routing-plugin` world in `wit/token-center.wit`; export
`group-routing-v1.plan(input-json)` and `group-routing-v1.observe(input-json)`,
both returning `result<string, string>`. Existing `plugin` world exports remain
unchanged; a routing-only component needs no traffic or provider exports.

Minimal manifest contribution (inside an executable plugin manifest):

```json
{
  "group_routing": {
    "version": "group-routing-v1",
    "schema": {"type": "object", "additionalProperties": false},
    "default": {}
  }
}
```

The installed strategy catalog exposes `id`, `version` (the ABI capability
version), `schema`, and `default`. Group configurations are validated against
the installed schema. The host caches compiled validators. Credentials and
write-only configuration are not supported by this contribution.

## Plan

Example input:

```json
{
  "tenant_id": "tenant", "seed": 42, "remaining_deadline_ms": 1000,
  "config": {},
  "candidates": [{
    "tenant_id": "tenant", "route_id": "route", "account_id": "account",
    "generation": 1, "health": "transient"
  }]
}
```

Health is one of `healthy`, `transient`, `hard_quota`, or `authentication`.
The host provides only already-authorized candidates; neither credentials nor
request/response bodies enter the routing component. The seed is stable for
the host's selection scope. It is not an authorization token.

Example output:

```json
{
  "candidates": [{
    "tenant_id": "tenant", "route_id": "route", "account_id": "account",
    "generation": 1, "allow_transient_probe": true,
    "cooldown_ms": 1000, "recovery_wait_ms": 500,
    "recheck_ms": 100, "stickiness": false
  }]
}
```

The result must be an exact permutation of all supplied candidate identities,
including tenant and credential generation: no filtering, duplication,
injection, or identity modification. `stickiness` opts a candidate into the
host's stable ordering tier. `allow_transient_probe` may be true only for a
transient candidate. Hard-quota/authentication states cannot be revived; their
cooldown fields do not override the host's hard evidence. A zero cooldown or
sticky ordering alone does not grant admission. Cross-process probe leases,
credential eligibility and final dispatch checks remain host-owned.

All fields are mandatory and unknown fields are rejected. At most 1024
candidates and 1 MiB of input/output JSON are accepted. Identifiers are
nonempty, at most 128 bytes, without control characters. Delay fields are
bounded by 300000 ms; `recovery_wait_ms` must additionally fit the supplied
remaining deadline. Host admission can impose stricter operational caps.
Plan requires a nonzero remaining scheduling deadline.

## Observe

Observation input replaces the plan's `candidates` array with one `candidate`
and an `outcome`. Other input fields are unchanged. Outcomes are `success`,
`transient_failure`, `hard_quota`, `authentication`, or `cancelled`. Output is
one directive object with the same fields as a plan candidate, without the
surrounding `candidates` array. Its identity must exactly match the input.
Hard-quota and authentication outcomes override stale input health during
validation and prohibit a transient probe recommendation.

Observations after a long stream may have `remaining_deadline_ms: 0`. They
still execute with a separate bounded computation allowance, but must return
`recovery_wait_ms: 0`: observation cannot extend the original wait budget.
There is no replay control in either output. Retry safety, visible output,
uncertain execution and lease updates are core decisions.

## Isolation and revision pinning

Each call has fuel, memory/table limits and at most 100 ms of execution time.
The host shares its existing eight-component process capacity, including
after caller cancellation. Scheduling waits at most 25 ms for capacity; the
entire batch snapshot and plan stage is capped at 250 ms and the frozen core
deadline. Health and group selection are read by one generation-bound batch,
not a serial query per candidate. Closed metric phases `group_routing_plan`
and `group_routing_observe` expose returned/error/timeout/cancellation outcomes.
Routing gets no network or KV access even if another contribution in the same
package declares those capabilities. Guest traps, malformed results and
timeouts are errors; the integration records native-policy fallback for the
same authorized candidates, never a rollback to an older plugin revision.

The request owner must retain its cloned `PluginRuntime` for both planning
and terminal observation. A runtime clone owns immutable compiled components
and manifests; changing package files or loading a newer runtime does not
change the old clone. Contributions are included in the application contract
digest. The integration test `pinned_runtime_keeps_old_plan_and_observe_after_package_upgrade`
loads distinct real Wasm versions and checks that the pinned request keeps old
plan and observe behavior while the new runtime uses the upgraded behavior.
Other tests cover the independent world, strict output parsing, deterministic
execution, input identity checks, bounded waits and fuel exhaustion.
