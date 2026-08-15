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
  `https://token-center.api.onetwo.website:24450/portal`.

The dedicated `24450` Envoy listener allows only `/v1`, `/self`, `/portal`,
`/ui-assets` and `/healthz`. It rejects `/operator` and `/internal`, and a wrong
SNI is reset. This is the correct public address. The same hostname on ordinary
port 443 is not the advertised gateway and was not a valid substitute in the
audit environment.

## What the next chart changes

The pending chart uses one bounded `pre-install,pre-upgrade` migration Job and
sets `MTC_RUN_MIGRATIONS_ON_START=false` in all Deployments. Argo CD translates
the existing Helm migration hook into `PreSync`; the last observed hook result
was `Reached expected number of succeeded pods`.

The dogfood database was v12 at audit time. The current working-tree binary
contains v13-v23. These migrations add columns, indexes and tables; none drops
or renames an existing application column or table, but that alone does **not**
make an old binary write-compatible. Starting at v22, every budget reservation,
settlement and cancellation must transactionally maintain the new rollup state.
An old pod that writes after v22 is applied will not do so. Therefore an ordinary
PreSync-migration/rolling-update sequence is a NO-GO for v22/v23. Because this
review instance is not carrying production traffic and old CPA remains the
production path, use two separate GitOps stages: Stage A sets Token Center
gateway/control/worker replicas to zero and waits for termination plus zero
active requests/jobs; Stage B pins the reviewed SHA/image, enables migration,
applies v12→v23 and starts only v23 pods. Do not combine these changes into one
Argo sync. After v22 is applied, do not restore request traffic to the v12 image.
Do not attempt to roll the database schema backward.

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

The GitHub `ci` workflow validates every `master` push first, then its
`publish-ghcr` matrix builds the service and importer and publishes both to
GHCR as `sha-<full commit>` plus the moving `master` tag. The publish job uses
the repository-scoped `GITHUB_TOKEN`, emits an SBOM and provenance, and records
the registry digest. A private repository can publish these packages; private
packages require an image pull credential, while public GHCR container packages
allow anonymous pulls.

The Forgejo workflow remains a Harbor fallback and builds the same service and
importer on `master`. Its date/short-SHA tag is a discovery label, not an
immutability boundary: this Harbor project currently permits tag replacement.
Discover the tag instead of guessing the UTC date, verify its digest, and deploy
as `repository:tag@sha256:digest`:

```bash
export MTC_REPO=/home/chenshuangfeng/Github/memeloop-token-center
export OPS_HOST=main.admin-test.lindongwu11.coder
MTC_SHA=$(git -C "$MTC_REPO" rev-parse HEAD)
MTC_SHORT=$(printf '%s' "$MTC_SHA" | cut -c1-7)

MTC_TAG=$(
  ssh "$OPS_HOST" \
    'curl --fail --silent "http://harbor-core.harbor.svc.cluster.local/api/v2.0/projects/library/repositories/memeloop-token-center/artifacts?with_tag=true&page_size=100"' \
  | jq -r --arg suffix "-$MTC_SHORT" \
      '[.[] | .tags[]?.name | select(endswith($suffix))] | unique | if length == 1 then .[0] else empty end'
)
test -n "$MTC_TAG"

ssh "$OPS_HOST" \
  "curl --fail --silent 'http://harbor-core.harbor.svc.cluster.local/api/v2.0/projects/library/repositories/memeloop-token-center/artifacts/$MTC_TAG'" \
  | jq '{digest, push_time, tags: [.tags[]?.name]}'
ssh "$OPS_HOST" \
  "curl --fail --silent 'http://harbor-core.harbor.svc.cluster.local/api/v2.0/projects/library/repositories/memeloop-token-center-importer/artifacts/$MTC_TAG'" \
  | jq '{digest, push_time, tags: [.tags[]?.name]}'
```

Both artifacts must exist. Record their digests in the release evidence. A
mutable date-only tag is never a deployment input.

## Gate 2: create an independent pre-upgrade recovery point

Back up PostgreSQL first, then mirror S3. Archive objects are content addressed
and written before their database reference is finalized, so an S3 mirror taken
after the database snapshot may contain harmless extra objects but must contain
every object referenced by that database snapshot.

Create and validate a custom-format logical PostgreSQL backup outside the K3s
storage control plane:

```bash
export OPS_HOST=main.admin-test.lindongwu11.coder
BACKUP_ROOT=/home/chenshuangfeng/Github/memeloop-token-center-backups
BACKUP_ID=$(date -u +%Y%m%dT%H%M%SZ)
BACKUP_DIR="$BACKUP_ROOT/$BACKUP_ID"
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

The dev app currently follows Token Center `master`, pins an old image and has
`migration.enabled=false`. The new chart disables startup migrations, so simply
changing its image would leave the dev database at v10 and is not a valid
canary. In one GitOps commit:

- pin the dev chart `targetRevision` to the exact `MTC_SHA`;
- set its image tag to the immutable `MTC_TAG`;
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
reviewed source revision (v23 in the current working tree). The fresh restricted
PostgreSQL gate has now applied v1–v23 without a gap and passed the locator,
budget-concurrency, generation-aggregate, entitlement and observability suites.
This clears the functional database gate, not the pending 141k-row migration
lock-time or imported-scale EXPLAIN/latency gate. Stop if readiness fails,
the migration Job fails, or the reported revision is not exactly `MTC_SHA`.

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

Update the operational Job's image tag to `MTC_TAG` before creating it. Record
the pre/post target request count, importer `staged`, `unmapped` and duplicate
counts, checkpoint watermark and source file modification time. Re-run once;
the second run must not increase target request or aggregate counts.

The following creates the immutable one-shot Job without rendering a Secret.
First verify its pinned node still matches the live CPAMP volume attachment:

```bash
test "$(ssh "$OPS_HOST" \
  'kubectl -n cliproxyapi get pod -l app.kubernetes.io/name=cpa-manager-plus -o jsonpath="{.items[0].spec.nodeName}"')" = haixia

sed -E \
  "s#(memeloop-token-center-importer:)[^[:space:]]+#\1${MTC_TAG}#" \
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
  "s#(memeloop-token-center-importer:)[^[:space:]]+#\1${MTC_TAG}#" \
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
   PreSync v12-to-v23 migration, restore the intended replica counts, and set the
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
6. Watch control and worker, then both v23 gateway replicas. The short review
   outage is intentional; do not try to preserve availability by overlapping a
   pre-v22 Token Center writer with v23.
7. Confirm the reviewed source's exact schema version (currently v23) and that
   request, aggregate, ledger and credential counts did not decrease during
   migration.
8. Verify the running `/version` revision and pod image digest, not only the
   manifest tag.
9. Run Playwright dogfooding locally and from the Windows Codex browser proxy.
   Public testing must include the path allowlist: `/operator` and `/internal`
   on `:24450` remain 404, while `/portal`, `/ui-assets`, `/self` authentication
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

curl --fail --silent https://token-center.api.onetwo.website:24450/healthz
test "$(curl --silent --output /dev/null --write-out '%{http_code}' \
  https://token-center.api.onetwo.website:24450/operator)" = 404
test "$(curl --silent --output /dev/null --write-out '%{http_code}' \
  https://token-center.api.onetwo.website:24450/internal/v1/schemas)" = 404
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
against v23. Push the revert normally:

```bash
export GITOPS_REPO=/home/chenshuangfeng/Github/k3s-gitops
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
deployment to it. Never restart the old Token Center binary against the v23
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
