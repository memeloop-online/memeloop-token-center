# Plugin configuration consistency

Configuration resolution intentionally permits bounded staleness, not strict
revocation. At the point a configuration snapshot is returned, its underlying
read must have started less than **five seconds** earlier. This bound assumes
the query uses the authoritative database with statement-level committed-read
visibility; replication lag is not covered. A request that already received a
snapshot is not retroactively revoked. Queueing and subsequent plugin execution
do not extend the snapshot's `freshness_deadline`, but the existing request path
does not enforce a fresh read again at dispatch.

`resolved_traffic_snapshot` returns the tenant, effective values, and each
plugin's selected source (`default`, `global`, `tenant`), row version and schema
digest. Default values use version zero and the installed schema digest.
Values and revision metadata are derived from the same configuration-layer
query; tenant overrides win over global settings. The compatibility method
`resolved_traffic_configurations` uses this same resolver and returns its values.

Within one runtime, cache lookup, invalidation and refill share a lock. An
invalidation replaces an opaque epoch token and removes affected entries. A
read holding the previous epoch may neither refill the cache nor return that
old result after the invalidation fence. It retries once; repeated invalidation
fails closed with a conflict. Each database read is independently bounded by
the five-second timeout. Tenant invalidation conservatively fences all ongoing
reads, while retaining unaffected cached entries.

Cache TTL starts **before** the database read, not when that read completes.
Expired reads are rejected rather than receiving a new TTL on refill. Reads
receive a checked, monotonic generation under the cache lock; older in-flight
reads cannot replace a cache entry with a later generation even when their
monotonic clock timestamps are identical. Generation exhaustion fails closed.
The entry-count and byte budgets include revision metadata and remain bounded.

Other processes have independent cache epochs. An API update invalidates only
its own runtime; another gateway may continue resolving the previous version
within the original five-second window. There is no claim of invalidation
broadcast or cluster-wide immediate visibility. Deterministic two-runtime tests
control statement results, read completion and monotonic time to exercise this
interleaving without sleep-based timing.

## Strict revocation remains an unimplemented integration boundary

A policy requiring immediate revocation must not rely on this cache contract.
It needs an authoritative revision/admission protocol shared with the relevant
write transaction, and a defined fence covering already-admitted or queued
requests. Neither a process-local epoch nor best-effort broadcast supplies that
guarantee. This change does not expose a `strict` flag or imply that such a mode
exists; core tenant, route and credential authorization remains authoritative.
