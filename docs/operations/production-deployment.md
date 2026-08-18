# Production deployment

This document is the production contract for the Helm chart. The chart deploys
the application only. PostgreSQL, S3-compatible object storage, their backups,
TLS certificates and secret delivery are external dependencies and must already
be healthy before the migration hook runs.

## Availability baseline

- Run at least two gateway replicas in different failure domains. Enable the
  gateway PDB only when there are two or more replicas.
- Run control and worker replicas according to the required control-plane and
  generation-job availability. PostgreSQL leases make multiple workers safe.
- Use a highly available PostgreSQL service (three instances for production is
  the normal baseline) or a managed PostgreSQL service with automatic failover.
- Use a production S3 service with versioning and replication. A single MinIO
  Deployment with one RWO PVC is a development fixture, not a production
  archive service.
- Add `topologySpreadConstraints` or `affinity` for the cluster's node and zone
  labels. The scheduler placing replicas on different nodes by chance is not an
  availability guarantee.
- Keep `terminationGracePeriodSeconds` longer than the longest admitted
  streaming or generation request, and validate graceful termination during a
  rollout.

The HPA is disabled by default. Before enabling it, enforce this invariant for
the maximum replica count, including surge replicas during an upgrade:

```text
sum(maximum replicas per role × config.databaseMaxConnections)
  + migration connections
  + operational reserve
  <= PostgreSQL or PgBouncer client connection budget
```

A PgBouncer transaction pool is recommended when the gateway is horizontally
scaled. Alert on pool wait time and acquisition failures, not only database CPU.

## Migration ownership

`migration.enabled=true` creates a bounded `pre-install,pre-upgrade` hook Job.
It runs only `memeloop-token-center migrate`. Every application Deployment sets
`MTC_RUN_MIGRATIONS_ON_START=false`, so schema changes have one owner and a
rolling update cannot multiply migration connections.

Do not disable the Job unless an external release pipeline applies the exact
same migration set before the new application image is started. A successful
schema migration does not prove application readiness; verify the new binary
against the migrated schema separately.

Schema v22 introduces transactionally maintained budget state, and schema v30
introduces the worker-indexed two-phase asynchronous-generation preparation
contract. Binaries older than these write barriers do not dual-write the new
state. In particular, mixing the pre-v30 archive-before-admission writer with a
v30 admission-before-archive writer can strand a reservation during an
idempotency race. A pre-upgrade hook followed by an ordinary rolling update
would therefore allow old pods to corrupt the new invariants. The review instance uses a two-stage
GitOps quiesce while production traffic remains on old CPA:

1. Stage A sets Token Center gateway, control and worker replicas to zero. Wait
   for every old pod to terminate and verify there are no active requests/jobs.
2. Stage B pins the reviewed source SHA and immutable image, enables the bounded
   migration hook, applies v12→v30, and starts only the new v30 pods.

For the first v30 deployment, every application role must use the Helm
`Recreate` strategy (or the equivalent explicit scale-to-zero barrier); no old
gateway may overlap a new gateway. Do not combine the replica-zero and new-image changes into one sync: the write
barrier must be observed before migration. After v22 is applied, the old binary
must never be restored to traffic. Application rollback means rolling forward a
compatible v22+ repair image while the barrier remains, or restoring a validated
pre-migration database into a new database before switching endpoints. A future
dual-write bridge release may replace this barrier; the current tree does not
contain one.

Partition maintenance creates request/request-event partitions for today through
eight days ahead. If PostgreSQL reports SQLSTATE `23514` because DEFAULT already
contains rows for one target day, that partition is rolled back to its own
savepoint, reported as blocked and retried on later worker passes; maintenance
continues with other dates and tables. No blocked row is moved or deleted
automatically. Alert on the warning/report and use the reviewed history-partition
backfill procedure to drain that exact day before relying on the next retry. A
regression test exists for this fail-soft path, but it had not yet been executed
against PostgreSQL at the time of this documentation snapshot.

Schema v24 creates compact terminal-request facts and UTC daily aggregates and
backfills them transactionally. Before enabling statistics traffic after a
legacy import, compare terminal request counts with `request_stats_facts`, run
`ops/reconcile-postgres-request-stats.sh` for any repaired/imported interval,
`ANALYZE` the fact and aggregate tables, and retain the resulting count and
EXPLAIN reports. The reconciliation command is dry-run by default and takes a
transaction advisory lock when `--apply` is supplied. Pruning compact statistics
is never implicit; it additionally requires `--confirm-prune` and fails closed
unless raw history has already been archived and removed by the separate
retention procedure.

## Health probes

The chart configures distinct startup, readiness and liveness paths. The current
server implements all three operational endpoints:

- `/livez`: process/event-loop health only; database or S3 outages must not
  trigger restart loops.
- `/readyz`: fail when the role cannot perform its required work. Gateway and
  control verify a bounded database query and the configured archive without
  writing on every probe. Concurrent probes are coalesced and briefly cached.
- `/healthz`: deprecated compatibility alias for `/livez`; new deployments use
  the distinct defaults above.

The public gateway ingress routes only the compatibility `/healthz` probe; it
does not route `/readyz` or `/livez`. The control ingress is disabled by default.
Enabling it requires a TLS secret, a Higress/ingress-nginx-compatible class and
one or more explicit `ingress.control.sourceRanges` office/VPN/NAT CIDRs;
every `/0` form is rejected. The chart renders the supported source
range and forced HTTPS annotations, which Higress also implements, and user
annotations cannot disable them. Do not expose a custom control route without
an equivalent source allowlist, VPN or SSO boundary.

## Network policy

The chart default is fail-closed for cross-namespace ingress and private egress:

- only namespace-local callers may reach port 8080; no ingress controller
  namespace is granted implicitly;
- DNS is allowed only to labelled kube-dns pods;
- public HTTP/HTTPS egress excludes private, carrier-grade NAT, loopback,
  link-local, benchmark and reserved ranges;
- PostgreSQL, S3 and an egress proxy have no default CIDR or port allowance.

Production values must explicitly select every cross-namespace dependency. The
following is the exact review-dogfood topology audited on 2026-08-14: Higress
gateway pods run in `higress-system`, while CNPG and MinIO run in
`memeloop-token-center-dev`. The active CPA subscription bridge runs in
`cliproxyapi`; public OAuth, price catalogs and other providers use the single
configured proxy. Reconfirm these labels before every rollout; they are not
portable defaults.

```yaml
networkPolicy:
  ingress:
    allowSameNamespace: false
    controllerNamespaces: []
    extraRules:
      - from:
          - namespaceSelector:
              matchLabels:
                kubernetes.io/metadata.name: higress-system
            podSelector:
              matchExpressions:
                - key: app
                  operator: In
                  values: [higress-gateway, higress-gateway-cp]
        ports: [{ protocol: TCP, port: 8080 }]
  egress:
    clusterDependencies:
      enabled: false
      cidrs: []
      ports: []
    outboundProxy:
      enabled: true
      cidrs: [100.64.0.2/32]
      ports: [{ protocol: TCP, port: 1080 }]
    publicInternet:
      enabled: false
      cidrs: []
      ports: []
    extraRules:
      - to:
          - namespaceSelector:
              matchLabels:
                kubernetes.io/metadata.name: memeloop-token-center-dev
            podSelector:
              matchLabels:
                cnpg.io/cluster: memeloop-token-center-pg
        ports: [{ protocol: TCP, port: 5432 }]
      - to:
          - namespaceSelector:
              matchLabels:
                kubernetes.io/metadata.name: memeloop-token-center-dev
            podSelector:
              matchLabels:
                app.kubernetes.io/name: minio
        ports: [{ protocol: TCP, port: 9000 }]
      - to:
          - namespaceSelector:
              matchLabels:
                kubernetes.io/metadata.name: cliproxyapi
            podSelector:
              matchLabels:
                app.kubernetes.io/name: cliproxyapi
        ports: [{ protocol: TCP, port: 8317 }]
```

The reviewed GitOps desired values now contain the explicit selectors, exact
CIDRs and ports above, and they pass the chart's strict render/schema contract.
Desired Git state is not live deployment evidence: confirm the Argo CD revision,
inspect the rendered NetworkPolicy, and verify allowed plus denied connectivity
before treating the public instance as conformant.

NetworkPolicy cannot select a DNS hostname. Provider hosts must either use the
public HTTPS rule, be reached through the authorized proxy, or have stable CIDRs
maintained as explicit rules. Test DNS, PostgreSQL, S3, OAuth and each private
provider from a policy-selected test pod before rollout.

## PostgreSQL and S3 requirements

PostgreSQL must provide TLS, failover, PITR, capacity monitoring and tested
restore procedures. Include `sslmode=verify-full` (and the appropriate CA
configuration) in the database URL. Do not point a production release at a
development namespace or database.

S3 must use HTTPS unless traffic is protected by an equivalent trusted service
mesh. Keep `config.s3.allowHttp=false` in production. Enable bucket versioning,
retention or object lock according to the data-retention policy, server-side
encryption and cross-failure-domain replication. Archive credentials should be
limited to the one bucket and required object operations.

Production S3/MinIO buckets **must** configure an
`AbortIncompleteMultipartUpload` lifecycle rule; one day after initiation is the
recommended limit. This rule aborts multipart sessions left by a process or node
crash. It is not an object-expiration rule. Never configure an ordinary TTL for
objects under `staging/`: successfully bound request, response, result, and asset
locators intentionally remain in that namespace for their full retention life.
The fenced archive reaper removes only unreferenced typed attempt segments.

Optional archive download rate, bandwidth and connection limits belong at
Higress. Do not add an application download limiter to compensate for a missing
gateway policy. Operators that need those limits configure and validate them at
the Kubernetes edge; they are not a Token Center release or cutover gate.

The Helm schema accepts only `config.archiveBackend=s3`. Although the binary's
filesystem and memory implementations remain useful for isolated tests, the
hardened chart has a read-only root filesystem, multiple independently scaled
roles and no local archive volume contract. It therefore rejects those backends
instead of rendering a deployment that appears valid but loses or strands
archive objects. Supply S3 credentials through the referenced external Secret;
the chart never renders credential values.

See [backup and restore](backup-and-restore.md) and
[secret management](secret-management.md) for the operational gates.

## Observability

The optional ServiceMonitor is disabled by default. The control role exposes
`/metrics` only to a service credential with `metrics:read`; gateway and worker
roles do not register it. When enabled, collect at least:

- request rate, errors and latency by protocol/route without credential labels;
- database pool in-use/idle/wait duration/acquisition failures;
- archive writes, gaps, latency and size;
- generation queue depth, oldest age, retries and lease recovery;
- reservation recovery and incremental-import lag;
- process RSS, CPU and restart count.

Add alerts and dashboards before a traffic cutover. A healthy Argo CD status and
successful `/healthz` response are necessary but not sufficient evidence.
