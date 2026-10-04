# Sticky Codex proxy groups: backend slice

This is an opt-in backend slice. Group management UI/API are not implemented;
this is not a complete product delivery or a production rollout. Other providers
and accounts without group configuration retain their single-proxy behavior.

## Private configuration

`MTC_TRANSPORT_PROXY_GROUPS` defaults to `[]`. Supply it through the existing
Secret mechanism, identically to gateway, control and OAuth worker processes.
Each entry has `account_id` (MTC account UUID), positive integer `version`, and
`proxies` (one to four distinct private IP-literal `socks5h://` URLs). Limits are
256 accounts and 256 KiB of JSON. Credentials may be part of the URLs; config
serialization omits this field and errors do not echo its contents.

Every member must expose a fixed exit. A mihomo priority/fallback group that
automatically returns to a preferred node can change the exit behind the same
SOCKS address and defeats application-level stickiness. Use fixed nodes or a
selection mode without automatic failback. Network policy must admit each
process that sends generation, catalog, quota, or OAuth traffic to every member.

## Dispatch and replay boundaries

Selection is a preallocated per-account atomic memory snapshot. Healthy requests
do not query a database, acquire a persistence permit, await a queue, rotate
members, or probe the preferred exit. Local CAS changes the exit only after the
existing Codex text transport proves a pre-delivery connection failure. Each
member is visited at most once per connection retry sequence; the existing
attempt limit and absolute deadline remain authoritative. No concurrency limit
is changed. There are no per-request persistence tasks or unbounded queues.

Unknown sends, HTTP/2 resets/GOAWAY, response/body failures, and streams that have
already produced output do not trigger group replay. The existing domain-specific
HTTP 400 behavior is unchanged. OAuth refresh remains one-shot behind its durable
dispatch fence. Catalog, image sends, health checks, quota reads and quota reset
operations share the local selection without adding retries.

Refresh preserves the operator's original credential proxy binding. A newer
credential snapshot invalidates old local CAS tickets and the client-cache epoch
while retaining the selected exit when the binding is unchanged. A configured
account with no proxy or a proxy outside its group fails closed, never DIRECT.
The 64-bit state encodes a positive 32-bit credential generation, 28-bit epoch,
and two two-bit indices. Exhaustion fails closed instead of wrapping old tickets.

## Optional persistence and consistency limits

One background task uses a separate, lazy, one-connection database pool and
awaits each account operation before starting the next, with a five-second delay
between sweeps. A sweep contains at most 256 accounts. Dirty state is coalesced
into one atomic slot per account. Slow/unavailable storage delays recovery and
saving; it cannot block selection or local fallback. No request waits for CAS
persistence. Database operations have no hard completion deadline: cancelling an
async timeout does not guarantee that the underlying database work has stopped.
A stuck operation can stall the entire persistence sweep; the worker does not
cancel it and launch replacement operations. Isolation from dispatch comes from
the independent pool and memory-only selection, not timeout cancellation.

The database holds only configuration fingerprints, numeric indices and
generations. Writes compare the observed durable state and current account
generation. Conflicts can drop an optional update; they never overwrite the
active local selection. A crash can lose an unsaved switch.

Background recovery may hydrate an untouched account before its first request.
If traffic arrives first, cold selection uses the credential's existing group
member. Late recovery never changes an active healthy local selection. All
operations within one AppState share that selection, but separate Pods can
choose different exits during startup, storage outages or independent failures.
There is **no instantaneous or eventual global-selection guarantee** for active
Pods. Shared storage is a best-effort restart record, not dispatch consensus.
Do not describe this slice as guaranteeing identical gateway and worker exits
when they run in separate Pods.

Group-version conflicts and newer credential generations discovered by background
refresh make stale snapshots fail closed. During storage outages, detection is
delayed; existing local snapshots remain available. Deploy group configuration
consistently across roles. Configuration changes require restart and an increased
group version. Removing a group restores the existing single-proxy path.

## Schema and validation

The only new production migration is **0116_transport_proxy_groups.sql**,
registered for both SQLite and PostgreSQL; Helm declares schema 116. Versions
0114 (#422) and 0115 (#455) remain owned by their PRs. The migration runner checks
the applied-version set, so gaps need no placeholder or dependency cherry-pick.
Historical migrations and deployed checksums are unchanged.

Validation runs only in GHA. Tests cover memory-only selection, stale tickets,
bounded configuration, epoch exhaustion, locked/unavailable database behavior,
SQLite/PostgreSQL recovery, ambiguous-delivery non-replay, client cache epochs,
and migration upgrade/replay. No local build/test or production account changes
are part of this slice.
