# Dogfood deployment readiness and rollback

This is the read-only audit and release procedure for the August 2026 dogfood
upgrade. It deliberately separates a review deployment from the later CPA
traffic cutover. Commands which mutate the cluster are runbook commands only;
none were executed during this audit.

## Decision

**No-go for an immediate upgrade.** A review-only rollout becomes a conditional
go after the backup, secret and network-policy gates below are satisfied. A
production CPA cutover remains no-go until PostgreSQL and archive storage meet
the HA/PITR requirements in `docs/operations/production-deployment.md`.

The blocking facts observed on 2026-08-13 UTC were:

- `memeloop-token-center-pg` has one instance. It has no CloudNativePG `Backup`
  or `ScheduledBackup`, no `firstRecoverabilityPoint`, and no
  `lastSuccessfulBackup`. Its Longhorn volume has two replicas, but a replicated
  PVC is not an independent backup.
- The dogfood archive is a single MinIO Deployment on one RWO Longhorn PVC. It
  has no versioning, replication or independent backup.
- The application, PostgreSQL bootstrap and archive Secrets inspected by name
  contain `kubectl.kubernetes.io/last-applied-configuration`. The annotation
  must be removed through the authorized secret workflow, and independently
  rotatable values must be rotated. Do not rotate the key pepper in place.
- The production-looking dogfood release uses a database and MinIO in the
  `memeloop-token-center-dev` namespace. That is acceptable only while old CPA
  remains the real traffic service; it is not a production data-plane design.
- The CPAMP SQLite source was still changing. Its file modification time was
  `2026-08-13T21:00:54Z`, while the import checkpoint watermark was
  `2026-08-13T13:08:08.090Z`. There is no importer CronJob. Incremental import is
  safe and supported, but it must be invoked explicitly.
- The one-shot importer Secret is currently absent from `cliproxyapi`, so an
  incremental import is not runnable until the password is copied through a
  short-lived Secret without exposing it in command output or argv.
- The present import is CPAMP usage/alias/price history, not a migration of the
  separate CPA session archive. The dogfood database has zero conversation
  observations, and 134,767 imported response references are explicit
  `gap://` placeholders. Historical aggregate/list dogfooding is possible, but
  historical request/response/session-body dogfooding is not complete.

Do not treat this no-go as a reason to change old CPA. CPA is currently healthy,
independent and must remain the production path until the approved weekend
window.

## Verified current topology

| Item | Observed state |
| --- | --- |
| Production dogfood Argo app | `Synced`, `Healthy`, source revision `64644034c575186d2931d62bcf7830a837c0188a` |
| Current dogfood image | tag `20260813-6464403`, digest `sha256:9c770f036fa8921ed92565049985ca5d57edd9cd9998ed7bbf2033e2ba08bef8` |
| Workloads | gateway `2/2`, control `1/1`, worker `1/1` |
| Dogfood database | `memeloop_token_center_dogfood`, schema v12, 509 MiB |
| Imported requests | 141,136 rows, 141,136 distinct request IDs; 141,134 are CPAMP-import reservations |
| Aggregates | 489 rows representing 141,136 requests |
| History layout | 133,208 rows remain in `request_records_default` (456 MiB); 7,928 are in the 2026-08-13 leaf |
| Archive | signed bucket list succeeds; two current objects total 1,676,774 bytes |
| Historical archive completeness | 134,767 imported response gaps and zero migrated conversation observations |
| Readiness dependency | the current S3 credential can list `memeloop-token-center-dogfood` |
| Old CPA | separate `cliproxyapi` namespace, Deployment, Ingresses and PVCs; CPAMP `usage.sqlite` is about 2.0 GiB and remains live |
| Import relationship | copy-only and checkpointed; Token Center Deployments do not mount any CPA PVC |

The current public/review surfaces are intentionally different:

- cluster-internal gateway:
  `http://memeloop-token-center-gateway.memeloop-token-center.svc.cluster.local:8080`;
- private operator ingress:
  `https://token-center.k3s.onetwo.website/operator`;
- public gateway/self portal:
  `https://token-center.api.onetwo.website/portal`.

The public TLS ingress on port 443 allows only `/v1`, `/self`, `/portal`,
`/ui-assets` and health/readiness paths. It returns 404 for `/operator` and
`/internal`; the operator surface remains on the private ingress only.
Higress/Ingress is also the sole owner of archive/asset download bandwidth,
request-rate and connection/concurrency limits. Token Center has no separate
application download limiter; its asset path remains responsible for
authentication, tenant isolation, exact range semantics and bounded streaming.

## What the next chart changes

The pending chart uses one bounded `pre-install,pre-upgrade` migration Job and
sets `MTC_RUN_MIGRATIONS_ON_START=false` in all Deployments. Argo CD translates
the existing Helm migration hook into `PreSync`; the last observed hook result
was `Reached expected number of succeeded pods`.

The dogfood database was v12 at the original audit time. The current working-tree binary
contains v13-v48. These migrations add columns, indexes and tables, plus bounded
data repair in v31, the fail-closed one-to-one legacy-key constraint in v33,
managed OAuth import identity in v34 and fenced archive-staging ownership in
v35. Versions v36-v42 add quarantine, Cloud entitlement audit, bounded
pagination, currency-safe observability, cancellation and plugin configuration.
Versions v43-v48 add normalized provider/route/credential groups, exact route
grants, upstream model catalogs, durable native Codex OAuth sessions, catalog
eligibility, immutable generation-route snapshots and relation-level CAS;
none drops or renames an existing application column or table. That alone
does **not** make an old binary write-compatible. Starting at v22, every budget
reservation, settlement and cancellation must transactionally maintain the new
rollup state. The v30 generation admission path likewise requires the new
`preparing` ownership/lease state, and v33 deliberately fails closed if one
legacy credential maps to multiple stable keys.

A fresh isolated PostgreSQL 16 and a PostgreSQL 17 database applied v1→v42
successfully; a fresh PostgreSQL 17 database subsequently applied v1→v48 and
passed the group-routing, model-catalog, native Codex OAuth, Cloud entitlement
and relation-CAS gates. This validates the empty-database
migration sequence and the tested runtime invariants; it does not measure locks
or latency while upgrading the imported dogfood snapshot from its deployed schema, and no live
formal v48 rollout has occurred. Therefore an ordinary PreSync-migration/rolling-update
sequence remains a NO-GO. Before authorizing Stage B, reconcile any schema-v33
uniqueness conflict and retain the production-size migration lock-time plus
final EXPLAIN/latency evidence. Because this review instance is not carrying
production traffic and old CPA remains the production path, use two separate
GitOps stages: Stage A sets Token Center gateway/control/worker replicas to zero
and waits for termination plus zero active requests/jobs; Stage B pins the
reviewed SHA/image, enables migration, applies the pending versions through v48 and starts only v48 pods.
Do not combine these changes into one Argo sync. After v22 is applied, do not
restore request traffic to the v12 image. Do not attempt to roll the database
schema backward.

The new probes have these contracts:

- `/livez` checks only that the process/event loop is responsive;
- `/readyz` executes `SELECT 1` and a signed, non-writing archive bucket list;
- `/healthz` remains a deprecated liveness alias.

Signed `ListObjectsV2` against the exact dogfood bucket returned HTTP 200 in the
audit, so the archive permission needed by `/readyz` exists. It does not prove
`GetObject`/`PutObject`; the existing archived objects and previous request
records prove those operations were used, and the dev canary must exercise a
fresh put/read before production dogfood rollout.

## Gate 1: identify immutable images

The GitHub `ci` workflow validates pull requests and every `master` push.
It defaults to read-only repository permissions and makes `packages: write`
available only to the master publication matrix. Publication cannot start until
all of these current-SHA gates succeed:

- repository secret and infrastructure-misconfiguration scan;
- `cargo-deny`, RustSec with no ignored advisory, and forbidden-dependency
  re-entry checks;
- Rust fmt, all-target/all-feature clippy with warnings denied, and the complete
  Rust suite against SQLite, PostgreSQL and the mock S3 service;
- TypeScript type checking, localization contracts, production web build and
  Cucumber.js/Playwright dogfooding;
- replay-safe SQLite and PostgreSQL migrations, CPA archive-delta tests, and the
  CPAMP initial/overlap/incremental/replay acceptance;
- OpenAPI/source-route/role contracts, Helm/Kubernetes packaging, and hardened
  importer/plugin-installer image contracts;
- the full 15-minute memory profile, including the 500 MiB asset stream.

The `publish-ghcr` matrix then builds the service, importer and plugin installer
exactly once for `linux/amd64`. Each image receives only the immutable
`sha-<full commit>` tag; no `latest` or moving `master` tag is release
evidence. BuildKit emits an OCI SBOM and maximum-mode provenance. CI scans the
exact published digest for high and critical OS/library vulnerabilities, proves
that the SHA tag resolves to the build output digest, checks the OCI revision
label and both attestations, and publishes a final three-image digest manifest
only after all matrix entries pass.

This describes the required gate, not a successful run. On 2026-08-21, run
`32472653829` for `2437415a30ef488cf84d4fd0ecdca58eee804414` failed before
any step started: every scheduled job had an empty step list, and GitHub's check
annotation said the account payment or spending limit prevented startup.
Therefore there is currently no acceptable GitHub CI, GHCR digest, SBOM,
provenance or scan evidence for this revision. Do not substitute a Coder,
Westlake, Forgejo or Harbor build. After billing is restored, push or rerun the
exact reviewed `master` SHA, require every job including
`verify-ghcr-release` to succeed, and retain the `ghcr-release-<sha>` artifact.

There is no repository-managed Forgejo/Harbor fallback. GitHub CI and the three
GHCR packages are the release authority. Select the `sha-<full commit>` images,
obtain their digests from the successful final release manifest, and deploy
service, importer and plugin installer only as
`repository:tag@sha256:digest` references:

```bash
: "${MTC_REPO:?Set MTC_REPO to the repository's absolute path}"
case "$MTC_REPO" in /*) ;; *) echo 'MTC_REPO must be absolute' >&2; exit 1;; esac
export OPS_HOST=main.admin-test.lindongwu11.coder
MTC_SHA=$(git -C "$MTC_REPO" rev-parse HEAD)
MTC_SERVICE_IMAGE="ghcr.io/linonetwo/memeloop-token-center:sha-${MTC_SHA}"
MTC_IMPORTER_IMAGE="ghcr.io/linonetwo/memeloop-token-center-importer:sha-${MTC_SHA}"
MTC_PLUGIN_INSTALLER_IMAGE="ghcr.io/linonetwo/memeloop-token-center-plugin-installer:sha-${MTC_SHA}"

# Authenticate to private GHCR packages without shell tracing, then inspect all three.
docker buildx imagetools inspect "$MTC_SERVICE_IMAGE"
docker buildx imagetools inspect "$MTC_IMPORTER_IMAGE"
docker buildx imagetools inspect "$MTC_PLUGIN_INSTALLER_IMAGE"

: "${MTC_SERVICE_DIGEST:?Set from the final release manifest (sha256:...)}"
: "${MTC_IMPORTER_DIGEST:?Set from the final release manifest (sha256:...)}"
: "${MTC_PLUGIN_INSTALLER_DIGEST:?Set from the final release manifest (sha256:...)}"
for digest in "$MTC_SERVICE_DIGEST" "$MTC_IMPORTER_DIGEST" \
  "$MTC_PLUGIN_INSTALLER_DIGEST"; do
  case "$digest" in
    sha256:[0-9a-f][0-9a-f][0-9a-f]*) ;;
    *) echo 'Every image digest must be a sha256 value' >&2; exit 1;;
  esac
done
MTC_SERVICE_IMAGE_PIN="${MTC_SERVICE_IMAGE}@${MTC_SERVICE_DIGEST}"
MTC_IMPORTER_IMAGE_PIN="${MTC_IMPORTER_IMAGE}@${MTC_IMPORTER_DIGEST}"
MTC_PLUGIN_INSTALLER_IMAGE_PIN="${MTC_PLUGIN_INSTALLER_IMAGE}@${MTC_PLUGIN_INSTALLER_DIGEST}"
```

For Helm releases, set `image.repository` and the strict `image.digest`
(`sha256:` plus 64 lowercase hexadecimal characters). A non-empty digest takes
precedence over `image.tag` for both application Deployments and the migration
Job, so the rendered workload uses `repository@digest`.

All three artifacts must exist and carry `org.opencontainers.image.revision`
equal to `MTC_SHA`. Record their separate digests in the release evidence. A moving
`master` tag is never a deployment input.

If the cluster must pull from Harbor, use only an independently approved and
verified registry synchronization/import procedure, then prove that each
destination artifact has the same platform manifest and revision label as its
GHCR source and pin the Harbor destination digest. This repository currently
contains no such synchronization procedure, so the mere presence of an old or
mutable Harbor tag is not release evidence; use GHCR with an image-pull Secret
until the external procedure and its digest-equivalence evidence are supplied.

## Gate 2: create an independent pre-upgrade recovery point

Back up PostgreSQL first, then mirror S3. Archive objects are content addressed
and written before their database reference is finalized, so an S3 mirror taken
after the database snapshot may contain harmless extra objects but must contain
every object referenced by that database snapshot.

Create and validate a custom-format logical PostgreSQL backup outside the K3s
storage control plane:

```bash
export OPS_HOST=main.admin-test.lindongwu11.coder
: "${MTC_BACKUP_ROOT:?Set MTC_BACKUP_ROOT to an absolute backup directory}"
case "$MTC_BACKUP_ROOT" in /*) ;; *) echo 'MTC_BACKUP_ROOT must be absolute' >&2; exit 1;; esac
BACKUP_ID=$(date -u +%Y%m%dT%H%M%SZ)
BACKUP_DIR="$MTC_BACKUP_ROOT/$BACKUP_ID"
install -d -m 0700 "$BACKUP_DIR/s3"

ssh "$OPS_HOST" '
  pod=$(kubectl -n memeloop-token-center-dev get pod \
    -l cnpg.io/instanceRole=primary -o jsonpath="{.items[0].metadata.name}")
  kubectl -n memeloop-token-center-dev exec -c postgres "$pod" -- \
    pg_dump --format=custom --compress=6 --no-owner --no-acl \
      memeloop_token_center_dogfood
' >"$BACKUP_DIR/postgres.dump"

test -s "$BACKUP_DIR/postgres.dump"
sha256sum "$BACKUP_DIR/postgres.dump" >"$BACKUP_DIR/postgres.dump.sha256"
ssh "$OPS_HOST" '
  pod=$(kubectl -n memeloop-token-center-dev get pod \
    -l cnpg.io/instanceRole=primary -o jsonpath="{.items[0].metadata.name}")
  kubectl -n memeloop-token-center-dev exec -i -c postgres "$pod" -- \
    pg_restore --list
' <"$BACKUP_DIR/postgres.dump" >"$BACKUP_DIR/postgres.contents"
test -s "$BACKUP_DIR/postgres.contents"
```

Record a non-secret manifest at the same recovery point:

```bash
ssh "$OPS_HOST" '
  pod=$(kubectl -n memeloop-token-center-dev get pod \
    -l cnpg.io/instanceRole=primary -o jsonpath="{.items[0].metadata.name}")
  kubectl -n memeloop-token-center-dev exec -c postgres "$pod" -- \
    psql -X -d memeloop_token_center_dogfood -P pager=off -c "
      SELECT max(version) AS schema_version, count(*) AS applied
        FROM schema_migrations;
      SELECT count(*) AS requests, count(DISTINCT id) AS distinct_requests,
             min(created_at) AS oldest_ms, max(created_at) AS newest_ms
        FROM request_records;
      SELECT count(*) AS aggregate_rows, coalesce(sum(requests),0) AS requests
        FROM usage_daily_aggregates;
      SELECT pg_current_wal_lsn() AS lsn, current_timestamp AS observed_at;
    "
' >"$BACKUP_DIR/database-manifest.txt"
```

For this small dogfood bucket, mirror current S3 contents through an SSH
port-forward. Secret values are passed through environment variables and never
printed. Keep this shell out of debug tracing (`set -x` must be off):

```bash
ssh -o ExitOnForwardFailure=yes -L 19000:127.0.0.1:19000 "$OPS_HOST" \
  'kubectl -n memeloop-token-center-dev port-forward --address 127.0.0.1 service/minio 19000:9000' \
  >"$BACKUP_DIR/minio-port-forward.log" 2>&1 &
MTC_PORT_FORWARD_PID=$!
trap 'kill "$MTC_PORT_FORWARD_PID" 2>/dev/null || true' EXIT HUP INT TERM

attempt=0
until curl --fail --silent http://127.0.0.1:19000/minio/health/live >/dev/null; do
  attempt=$((attempt + 1))
  test "$attempt" -lt 30
  sleep 1
done

MTC_BACKUP_S3_ACCESS_KEY=$(
  ssh "$OPS_HOST" \
    'kubectl -n memeloop-token-center get secret memeloop-token-center-secrets -o jsonpath="{.data.s3-access-key}"' \
  | base64 -d
)
MTC_BACKUP_S3_SECRET_KEY=$(
  ssh "$OPS_HOST" \
    'kubectl -n memeloop-token-center get secret memeloop-token-center-secrets -o jsonpath="{.data.s3-secret-key}"' \
  | base64 -d
)
export MTC_BACKUP_S3_ACCESS_KEY MTC_BACKUP_S3_SECRET_KEY

docker run --rm --network host \
  --env MTC_BACKUP_S3_ACCESS_KEY --env MTC_BACKUP_S3_SECRET_KEY \
  --volume "$BACKUP_DIR/s3:/backup" \
  --entrypoint /bin/sh \
  harbor.k3s.onetwo.website/quay-io/minio/mc:RELEASE.2025-08-13T08-35-41Z \
  -ec '
    mc --config-dir /tmp/mc alias set source http://127.0.0.1:19000 \
      "$MTC_BACKUP_S3_ACCESS_KEY" "$MTC_BACKUP_S3_SECRET_KEY" >/dev/null
    mc --config-dir /tmp/mc mirror --overwrite \
      source/memeloop-token-center-dogfood /backup
    mc --config-dir /tmp/mc du source/memeloop-token-center-dogfood
  ' >"$BACKUP_DIR/s3-manifest.txt"

unset MTC_BACKUP_S3_ACCESS_KEY MTC_BACKUP_S3_SECRET_KEY
find "$BACKUP_DIR/s3" -type f -print0 | sort -z | xargs -0 sha256sum \
  >"$BACKUP_DIR/s3.sha256"
test -s "$BACKUP_DIR/s3.sha256"
```

For the audited recovery point this must report at least the two objects and
1,676,774 bytes observed before deployment. Store the completed backup in a
second failure domain before calling it a production backup. The local copy is
only the minimum gate for review dogfooding.

## Gate 3: fix Secrets and make cross-namespace egress explicit

Before rollout:

1. Remove the last-applied annotation through the authorized secret-management
   workflow without printing it.
2. Rotate the database credential, S3 credential and bootstrap/review service
   credentials. Use a dual-credential rollout where the backing service allows
   it. Do not rotate `key-pepper` until versioned peppers exist.
3. Replace MinIO root credentials with a dogfood-bucket-specific principal that
   has only List/Get/Put/Delete/multipart permissions needed by the archive.
4. Confirm no value is shared unintentionally between the dev and dogfood
   release.

The current chart has no private-cluster compatibility egress. Production
dogfood must explicitly select the audited Higress, CNPG and MinIO pods. The
selectors below match the cluster as observed on 2026-08-14; re-read labels
before committing because a renamed gateway or data service must fail closed:

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
    # Public provider traffic uses the configured egress proxy.
    publicInternet:
      enabled: false
      cidrs: []
      ports: []
    outboundProxy:
      enabled: true
      cidrs: [100.64.0.2/32]
      ports: [{ protocol: TCP, port: 1080 }]
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
```

DNS remains allowed to labelled CoreDNS pods. The cluster has no
`ingress-nginx` namespace; `IngressClass/nginx` is controlled by Higress, and
only `higress-system` pods with `app` equal to `higress-gateway` or
`higress-gateway-cp` need port 8080 access. At audit time the live release still
had `egress: [{}]` and an ingress rule without `from`; these exact values had
passed local Helm/schema/kubeconform checks but had not been applied through
GitOps. Do not record Gate 3 as passed until the live object is restrictive and
the dependency probes succeed.

## Gate 4: dev canary before the imported dogfood database

At the 2026-08-13 audit, the dev app followed Token Center `master`, pinned an
old image and had `migration.enabled=false`; its database was v10. The new chart
disables startup migrations, so simply changing the image is not a valid
canary. In one GitOps commit:

- pin the dev chart `targetRevision` to the exact `MTC_SHA`;
- set its image to the immutable `MTC_SERVICE_IMAGE_PIN`;
- set `migration.enabled=true`;
- retain one gateway and its separate `memeloop_token_center` database/bucket.

Wait for the PreSync migration and all three dev workloads:

```bash
ssh "$OPS_HOST" "kubectl -n argocd get application memeloop-token-center-dev -w"
ssh "$OPS_HOST" \
  'kubectl -n memeloop-token-center-dev rollout status deployment/memeloop-token-center-gateway --timeout=10m'
ssh "$OPS_HOST" \
  'kubectl -n memeloop-token-center-dev rollout status deployment/memeloop-token-center-control --timeout=10m'
ssh "$OPS_HOST" \
  'kubectl -n memeloop-token-center-dev rollout status deployment/memeloop-token-center-worker --timeout=10m'
```

Then verify `/version`, `/livez`, `/readyz`, UI assets and one disposable
credential's mocked or non-billable proxy/archive/accounting path through a
port-forward. The dev database must report the schema version declared by the
reviewed source revision (v48 in the current working tree). The fresh isolated
PostgreSQL 16/17 v1→v42 migration, PostgreSQL 17 v1→v48 migration and focused
PostgreSQL gates have passed. They do
not clear the pending production-snapshot migration lock-time or final imported-scale
EXPLAIN/latency, current-SHA 15-minute memory, or live rollout gates. Stop if
those required release artifacts have not been retained, readiness fails, the
migration Job fails, or the reported revision is not exactly `MTC_SHA`.

## Incremental CPAMP refresh

The importer is idempotent and re-reads a 24-hour overlap from its checkpoint.
It uses deterministic request identities and an advisory lock, so it can run
while CPA remains live. This is an interim refresh, not the final cutover
boundary.

Use the newly built immutable importer image. Its Job must:

- run in `cliproxyapi` on `haixia`, where the RWO CPAMP PVC is currently
  attached;
- mount `cpa-manager-plus-data` read-only;
- target `memeloop_token_center_dogfood`;
- leave `CPAMP_RESET_IMPORT=false`;
- use a short-lived Secret created without a last-applied annotation;
- be deleted, together with the short-lived Secret, after logs and counts are
  recorded.

Never place the database password in `--from-literal`, shell tracing, a rendered
Job file or command output. Copy its already encoded Secret data through stdin:

```bash
ssh "$OPS_HOST" '
  set -eu
  encoded=$(kubectl -n memeloop-token-center-dev get secret \
    memeloop-token-center-pg-app -o jsonpath="{.data.password}")
  test -n "$encoded"
  printf "%s" "{\"apiVersion\":\"v1\",\"kind\":\"Secret\",\"metadata\":{\"name\":\"memeloop-token-center-cpamp-import\",\"namespace\":\"cliproxyapi\"},\"type\":\"Opaque\",\"data\":{\"database-password\":\"$encoded\"}}" \
    | kubectl create -f -
  unset encoded
'
```

Update the operational Job's image to `MTC_IMPORTER_IMAGE_PIN` before creating it. Record
the pre/post target request count, importer `staged`, `unmapped` and duplicate
counts, checkpoint watermark and source file modification time. Re-run once;
the second run must not increase target request or aggregate counts.

The following creates the immutable one-shot Job without rendering a Secret.
First verify its pinned node still matches the live CPAMP volume attachment:

```bash
test "$(ssh "$OPS_HOST" \
  'kubectl -n cliproxyapi get pod -l app.kubernetes.io/name=cpa-manager-plus -o jsonpath="{.items[0].spec.nodeName}"')" = haixia

sed -E \
  "s#^([[:space:]]*image:)[[:space:]].*#\1 ${MTC_IMPORTER_IMAGE_PIN}#" \
  "$MTC_REPO/ops/kubernetes/cpamp-import-job.yaml" \
  | ssh "$OPS_HOST" 'kubectl create -f -'

ssh "$OPS_HOST" \
  'kubectl -n cliproxyapi wait --for=condition=complete --timeout=3600s job/memeloop-token-center-cpamp-import'
ssh "$OPS_HOST" \
  'kubectl -n cliproxyapi logs job/memeloop-token-center-cpamp-import'
```

Capture target evidence without selecting request bodies or credentials:

```bash
ssh "$OPS_HOST" '
  pod=$(kubectl -n memeloop-token-center-dev get pod \
    -l cnpg.io/instanceRole=primary -o jsonpath="{.items[0].metadata.name}")
  kubectl -n memeloop-token-center-dev exec -c postgres "$pod" -- \
    psql -X -d memeloop_token_center_dogfood -P pager=off -c "
      SELECT count(*) AS requests, count(DISTINCT id) AS distinct_requests
        FROM request_records;
      SELECT count(*) AS aggregate_rows, coalesce(sum(requests),0) AS requests
        FROM usage_daily_aggregates;
      SELECT tenant_external_id, source, watermark_ms, imported_events, updated_at
        FROM cpamp_import_checkpoints
       ORDER BY tenant_external_id, source;
    "
'
```

Delete and recreate the Job once, then repeat the count query. Only after the
second-run zero-change evidence is saved, remove both operational objects:

```bash
ssh "$OPS_HOST" \
  'kubectl -n cliproxyapi delete job memeloop-token-center-cpamp-import --wait=true'
sed -E \
  "s#^([[:space:]]*image:)[[:space:]].*#\1 ${MTC_IMPORTER_IMAGE_PIN}#" \
  "$MTC_REPO/ops/kubernetes/cpamp-import-job.yaml" \
  | ssh "$OPS_HOST" 'kubectl create -f -'
ssh "$OPS_HOST" \
  'kubectl -n cliproxyapi wait --for=condition=complete --timeout=3600s job/memeloop-token-center-cpamp-import'
ssh "$OPS_HOST" \
  'kubectl -n cliproxyapi logs job/memeloop-token-center-cpamp-import'
ssh "$OPS_HOST" \
  'kubectl -n cliproxyapi delete job memeloop-token-center-cpamp-import --wait=true'
ssh "$OPS_HOST" \
  'kubectl -n cliproxyapi delete secret memeloop-token-center-cpamp-import --wait=true'
```

The final weekend import is stricter: first establish a CPA write barrier, take
an approved consistent SQLite/PVC backup, run the delta, run it again for the
zero-change proof, and only then shift traffic. A changing SQLite file is not a
cutover boundary.

## Production dogfood rollout

Only after Gates 1-4 and the backup checks pass:

1. **Stage A -- quiesce only the Token Center review instance.** Commit and sync
   gateway, control and worker replicas at zero. Do not change or stop old CPA;
   it remains the production traffic path. Wait until all three Token Center
   workloads have terminated, no gateway request is active, and no worker job
   remains active or leased. Do not combine this write barrier with migration or
   the new image in one GitOps commit.
2. **Stage B -- migrate and start only the reviewed binary.** In a separate
   commit, pin the exact Token Center source SHA and immutable image, enable the
   PreSync migration through v48, restore the intended replica counts, and set the
   explicit NetworkPolicy selectors and probe paths. An old Token Center binary
   must never start after v22 has been applied.
3. Push each stage's commit to the remote actually consumed by Argo CD. This
   checkout's `cluster/master` was the deployed history; its `origin/master`
   was 35 commits behind at audit time. Do not push a stale `origin/master` or
   force either branch.
4. Record both stage commit SHAs, the commit before Stage A, and the validated
   pre-migration backup identity. For compatibility with the commands below,
   set `DEPLOY_GITOPS_SHA` to the Stage B commit and `ROLLBACK_GITOPS_SHA` to the
   commit before Stage A.
5. Watch the Stage B PreSync migration Job. A failure must prevent Deployment
   rollout and leave the Token Center review roles at zero; old CPA continues
   serving production traffic.
6. Watch control and worker, then both v48 gateway replicas. The short review
   outage is intentional; do not try to preserve availability by overlapping a
   pre-v22 Token Center writer with v48.
7. Confirm the reviewed source's exact schema version (currently v48) and that
   request, aggregate, ledger and credential counts did not decrease during
   migration.
8. Verify the running `/version` revision and pod image digest, not only the
   manifest tag.
9. Run the TypeScript Cucumber.js browser dogfooding suite locally and from the
   Windows Codex browser proxy. Playwright is only the browser driver inside steps.
   Public testing must include the path allowlist: `/operator` and `/internal`
   on the public origin remain 404, while `/portal`, `/ui-assets`, `/self` authentication
   and model routes work.

Useful read-only checks:

```bash
ssh "$OPS_HOST" '
  kubectl -n argocd get application memeloop-token-center \
    -o custom-columns="SYNC:.status.sync.status,HEALTH:.status.health.status,REVISION:.status.sync.revision"
  kubectl -n memeloop-token-center get pod,svc,ingress,networkpolicy,pdb
  kubectl -n memeloop-token-center get endpointslices \
    -l app.kubernetes.io/name=memeloop-token-center
  kubectl -n memeloop-token-center top pod
'

curl --fail --silent https://token-center.api.onetwo.website/healthz
test "$(curl --silent --output /dev/null --write-out '%{http_code}' \
  https://token-center.api.onetwo.website/operator)" = 404
test "$(curl --silent --output /dev/null --write-out '%{http_code}' \
  https://token-center.api.onetwo.website/internal/v1/schemas)" = 404
```

The baseline already contains one SQLx warning where a gateway connection took
5.2 seconds to acquire. Treat repeated pool acquisition warnings or growth in
latency as a rollout signal even if Argo remains Healthy.

## Abort and rollback

Abort the rollout when any of these occurs:

- the migration hook does not succeed within its 600-second deadline;
- `/readyz` fails from any new pod, or the EndpointSlice loses all Ready gateway
  addresses;
- the running revision/digest differs from the recorded artifact;
- credential identity, permissions, balances, request count or aggregate count
  changes unexpectedly;
- archive list/read/write fails or archive gaps grow for new requests;
- database pool acquisition warnings repeat, PostgreSQL connections approach
  the 50-connection budget, RSS approaches the pod limit, or 5xx/latency rises;
- the old CPA health changes. The Token Center rollout is not authorized to
  restart, scale or reconfigure CPA.

Rollback through Git, not `argocd app rollback`: automated self-heal would
reapply Git. Reverting Stage B returns the review roles to the Stage A
zero-replica barrier; it does not authorize the old Token Center image to run
against v48. Push the revert normally:

```bash
: "${GITOPS_REPO:?Set GITOPS_REPO to the GitOps repository's absolute path}"
case "$GITOPS_REPO" in /*) ;; *) echo 'GITOPS_REPO must be absolute' >&2; exit 1;; esac
git -C "$GITOPS_REPO" status --short
git -C "$GITOPS_REPO" revert --no-edit "$DEPLOY_GITOPS_SHA"
git -C "$GITOPS_REPO" push cluster master

ssh "$OPS_HOST" 'kubectl -n argocd get application memeloop-token-center -w'
ssh "$OPS_HOST" \
  'kubectl -n memeloop-token-center rollout status deployment/memeloop-token-center-gateway --timeout=10m'
```

For a pre-v22 release, reverting the chart restores the prior image tag without
deleting additive schema or data. After v22 has been applied, reverting to the
v12 image is **not** a valid traffic rollback because old writers do not maintain
budget rollups. Scale Token Center roles back to zero, keep production traffic on
old CPA, and roll forward a compatible v22+ repair image; alternatively restore
the validated pre-migration backup into a new database and switch a compatible
deployment to it. Never restart the old Token Center binary against the v48
database. Preserve failed pod logs and the release interval for reconciliation.

If a migration or application defect corrupts the database, do not restore in
place. Create a new database, restore the validated dump, compare counts and
sample archives, then switch a newly issued database credential only after
approval. Route review clients back to the old Token Center image or route real
clients back to CPA while the restored database is validated.

For the later CPA cutover, rollback means routing clients back to the still
running CPA endpoints. Token Center traffic during the attempted cutover must be
retained for reconciliation; it must never be deleted to make counts appear to
match.
