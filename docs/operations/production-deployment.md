# Production deployment

This document defines the production contract for the Memeloop Token Center
Helm chart. The chart deploys the application roles and a bounded database
migration Job. PostgreSQL, S3-compatible object storage, TLS certificates,
secret delivery, ingress and their backups are external dependencies.

## Release inputs

- Pin the application image by immutable digest. Do not deploy a mutable tag.
- Keep `migration.schemaVersion` equal to the highest contiguous migration
  bundled with the image. The chart contract rejects version drift.
- Supply database, archive and provider credentials through referenced Secrets.
  Do not place secret values in Git, Helm values or rendered manifests.
- Use a highly available PostgreSQL service and a production S3-compatible
  service with TLS, versioning, replication and tested restore procedures.

The normal network shape has one cluster-internal service and separately
managed public gateway and operator addresses. Public gateway traffic and
operator traffic must use different ingress policies. Operator access requires
TLS plus an explicit office/VPN/NAT source allowlist or an equivalent SSO
boundary; the chart rejects an unrestricted operator source range.

## Application roles

The chart separates the service into three roles:

- `gateway` accepts compatible AI API traffic and performs admission,
  routing, quota accounting and streaming.
- `control` serves the operator and self-service APIs, metrics and catalog
  synchronization.
- `worker` performs asynchronous generation, archive and maintenance work.

Run at least two gateway replicas in different failure domains and enable its
PodDisruptionBudget only with two or more replicas. Size control and worker
replicas for the desired availability and queue latency. Configure topology
spread or affinity explicitly; incidental scheduler placement is not an
availability guarantee.

`MTC_GATEWAY_BODY_READ_CONCURRENCY` (Helm
`config.gatewayBodyReadConcurrency`) independently bounds bodies being
buffered by each gateway process. It defaults to 1024 and accepts 1 through
8192. When exhausted, a gateway returns HTTP 503 before reading a request
body; it is not a credential rate/concurrency rejection and never returns
HTTP 429. The permit ends as soon as the bounded body read completes, times
out or fails, rather than spanning proxy execution. Synchronous image requests
retain their separate two-request lifecycle limit.

Before enabling an HPA, keep the maximum application connection demand,
including rollout surge, migration connections and an operational reserve,
within the PostgreSQL or PgBouncer connection budget. Alert on database pool
wait time and acquisition failures as well as database CPU.

## Migration ownership and rollout

With `migration.enabled=true`, a bounded `pre-install,pre-upgrade` hook Job
runs only `memeloop-token-center migrate`. Application Deployments set
`MTC_RUN_MIGRATIONS_ON_START=false`, so schema changes have one owner.

Do not disable the Job unless an external release pipeline applies the exact
same migration set before the new application image starts. If a migration
introduces a write barrier that is not backward compatible, first scale every
application role to zero and verify that old pods and active work have drained.
Apply the migration and start only the new image in a separate synchronization
step. Do not overlap writers across such a barrier.

A successful migration is not application readiness evidence. Verify the new
binary against the migrated schema before routing traffic. After a
non-backward-compatible barrier, application rollback means deploying a
compatible repair image or restoring a validated pre-migration database to a
new database and switching endpoints.

Request and request-event partition maintenance creates partitions ahead of
current traffic. Alert on blocked partition maintenance and use the documented
backfill procedure for the affected day; the worker does not move or delete
blocked rows automatically. After a legacy import or repair, reconcile compact
request facts and aggregates for the affected interval and run `ANALYZE`.
Pruning raw history is a separate, explicit retention operation.

## Health and readiness

- `/livez` reports process and event-loop health. Dependency outages must not
  cause restart loops.
- `/readyz` checks the bounded dependencies required by the active role.
- `/healthz` is a compatibility alias for `/livez`.

The public gateway ingress may expose `/healthz`; it must not expose
`/readyz`, `/livez` or control APIs. Keep the termination grace period
longer than the longest admitted streaming or generation request and validate
graceful termination during rollout.

## Network policy

The chart defaults are fail-closed for cross-namespace ingress and private
egress. Production values must explicitly select:

- the ingress-controller namespace and pods that may reach the service;
- PostgreSQL and S3 destinations and ports;
- DNS service pods;
- an outbound proxy, if provider traffic uses one; and
- any private provider endpoints.

Do not copy namespace labels, CIDRs or pod selectors from another cluster.
Resolve them from the target cluster and validate both allowed and denied
connectivity before rollout. A NetworkPolicy cannot select a DNS hostname:
public providers must use the authorized public HTTPS rule or outbound proxy,
while private providers need stable, explicitly maintained destinations.

Application download throttling is intentionally not part of Token Center.
Configure download rate, bandwidth and connection limits at Higress or the
selected Kubernetes ingress layer.

## PostgreSQL and archive requirements

PostgreSQL must provide TLS verification, automatic failover, point-in-time
recovery, capacity monitoring and a tested restore path. Use
`sslmode=verify-full` with the appropriate CA configuration.

Production chart deployments use `config.archiveBackend=s3`. Filesystem,
SQLite and in-memory backends are test fixtures, not production storage.
Production S3-compatible storage must use HTTPS, server-side encryption,
versioning or object lock as required by policy, replication and least-privilege
bucket credentials.

Configure an `AbortIncompleteMultipartUpload` lifecycle rule for abandoned
multipart sessions. Do not apply an ordinary object-expiration TTL to active
archive prefixes: successfully bound request, response, result and asset
objects remain there for their full retention period.

See [backup and restore](backup-and-restore.md) and
[secret management](secret-management.md) for the remaining operational gates.

## Observability and release evidence

The optional ServiceMonitor is disabled by default. The control role exposes
`/metrics` only to a service credential with `metrics:read`. Gateway and worker
roles do not register it. The handler emits Prometheus text format 0.0.4 and
sets `Cache-Control: no-store`; a scrape performs bounded `/proc/self` reads,
advances the jemalloc statistics epoch and performs one bounded database
aggregate query. It does not enumerate tenants, credentials, models, URLs,
request IDs or error text.

The fixed-label runtime series are:

- `memeloop_token_center_http_active_requests` for handlers that have not yet
  produced response headers, plus bounded request/error/duration series;
- `memeloop_token_center_active_streams{kind}` for `proxy_response` and
  `request_events` streams that still own their background lifecycle;
- `memeloop_token_center_upstream_active_requests{provider,operation}` for an
  upstream exchange whose response has not been consumed or released;
- `memeloop_token_center_background_work_items{queue,state="active"}` for
  request-event streams, gateway body reads, proxy lifecycle capacity and
  response-archive stream capacity;
- `memeloop_token_center_generation_jobs{status}` and
  `memeloop_token_center_db_pool_connections{state}` for durable queue and pool
  state;
- `process_resident_memory_bytes`, `process_cpu_seconds_total` and
  `process_start_time_seconds` from the current Linux process;
- `memeloop_token_center_allocator_bytes{state}` for jemalloc `allocated`,
  `active`, `resident`, `mapped` and `retained`; and
- `memeloop_token_center_component_memory_bytes{component}` for request and
  response buffers, stream usage capture and the reserved archive multipart
  upper bound, alongside aggregate plugin cache bytes/entries and loaded
  plugin count. Component accounting is a diagnostic partition, not an
  allocator reconciliation: shared library buffers and Tokio/HTTP internals
  remain visible only in allocator and RSS totals.

At minimum, collect and alert on:

- request rate, errors and latency by protocol and route;
- session-level active requests, errors, latency, tokens and cost;
- database pool use, wait duration and acquisition failures;
- archive writes, gaps, latency and size;
- generation queue depth, oldest age, retries and lease recovery;
- quota reservation recovery and incremental-import lag; and
- process RSS, CPU and restart count.

### Controlled runtime diagnostics

Runtime profiling is absent by default. Set
`MTC_RUNTIME_PROFILING_ENABLED=true` only on an internal control deployment to
register the following routes. They are not registered on gateway/worker roles
and every request still requires a service credential with `metrics:read`.
Keep them off public ingress; no NodePort, hostPort or alternate public listener
is required or supported.

| Route | Output and bounds |
| --- | --- |
| `GET /internal/v1/diagnostics/runtime` | `no-store` JSON snapshot of process CPU/RSS/uptime, jemalloc totals and active diagnostic limits. |
| `POST /internal/v1/diagnostics/cpu-profile?seconds=10` | Linux pprof sampling at 99 Hz, returned as a Google pprof protobuf (`application/vnd.google.protobuf`) attachment for offline analysis. Duration defaults to 10 seconds and is restricted to 1–30 seconds; report generation has 15 seconds of grace and output is capped at 32 MiB. |
| `POST /internal/v1/diagnostics/heap-profile?seconds=10` | glibc jemalloc sampled heap dump returned as an `application/octet-stream` attachment compatible with `jeprof`. Duration is restricted to 1–30 seconds and output to 64 MiB. The temporary `/tmp` dump uses a random name and is removed after bounded reading. |

CPU and heap collection share one process-wide singleflight. A concurrent
request returns 409. The singleflight remains owned by the blocking capture
task if an HTTP client disconnects or the response deadline expires; captures
never invoke a shell. CPU sampling runs only during the requested interval.
Heap profiling support is compiled into glibc releases with sampling every
approximately 512 KiB, but `prof_active` is false until an authenticated
capture starts and its previous state is restored by an RAII guard. Release
builds strip debug sections while retaining the native symbol table required
for useful CPU profiles and derived flamegraphs. Profiling deliberately adds CPU/allocation
sampling overhead during a capture, so use the shortest interval that answers
the incident question and do not leave the route switch enabled routinely.

Verify the exposure boundary with an anonymous request (401 on enabled control,
404 on gateway and when disabled), a credential lacking `metrics:read` (403),
then download each profile with a 1-second capture before relying on it during
an incident. Treat the downloads as sensitive operational artifacts because
native symbol names and allocation stacks describe implementation details.

Before routing production traffic, retain evidence for the pinned source SHA,
image digest, chart render/schema checks, migration completion, readiness,
allowed and denied network tests, provider smoke tests, archive round trips and
rollback rehearsal. Healthy GitOps status and a successful health probe are
necessary but not sufficient.
