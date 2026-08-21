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
`/metrics` only to a service credential with `metrics:read`. At minimum,
collect and alert on:

- request rate, errors and latency by protocol and route;
- session-level active requests, errors, latency, tokens and cost;
- database pool use, wait duration and acquisition failures;
- archive writes, gaps, latency and size;
- generation queue depth, oldest age, retries and lease recovery;
- quota reservation recovery and incremental-import lag; and
- process RSS, CPU and restart count.

Before routing production traffic, retain evidence for the pinned source SHA,
image digest, chart render/schema checks, migration completion, readiness,
allowed and denied network tests, provider smoke tests, archive round trips and
rollback rehearsal. Healthy GitOps status and a successful health probe are
necessary but not sufficient.
