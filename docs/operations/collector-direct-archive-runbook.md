# Collector-direct archive migration

Use this path with `cpa-session-archive` 0.8 or newer. It talks to the collector
API directly and does not depend on CPA product behavior. The older CPA plugin
path remains an input adapter for historical migrations only.

## Isolation and source identity

The collector API has no Authorization middleware. Do not expose it through an
Internet ingress and do not pass a CPA token to `--collector-direct`. Run the
export Job in the source cluster and restrict a migration-only Service with a
default-deny NetworkPolicy so only the Job pod label can connect. Use cluster
TLS, including an mTLS sidecar or service-mesh policy when the cluster provides
it, or explicitly allow exactly one private Service DNS name with
`--private-http-host`.

Keep the source URL byte-for-byte stable for the baseline and all deltas. A
practical layout is one migration-only Service whose selector initially targets
the isolated offline collector and later targets the active collector. Review
the selector change and endpoints before applying it. Never copy the archive
database between machines: create the consistent Longhorn clone in the same
cluster and mount it read-only into the offline collector.

The Job needs a private evidence PVC. Run these checks inside the Job pod:

```sh
set -eu
umask 077
test -d /evidence
test "$(stat -c %a /evidence)" -le 700
getent ahostsv4 cpa-session-archive-migration.cpa.svc.cluster.local
```

## Offline baseline

Start the isolated collector with
`ARCHIVE_ALLOW_OFFLINE_FULL_SNAPSHOT=true`. Confirm that `/v1/stats` advertises
`session-snapshot-cursor-v1` and `offline_full_snapshot_enabled: true`; the
exporter checks both again before it creates a snapshot.

```sh
set -eu
umask 077
SOURCE_HOST=cpa-session-archive-migration.cpa.svc.cluster.local
SOURCE_URL=http://${SOURCE_HOST}:8080
python3 ops/export-cpa-session-archive-delta.py \
  --collector-direct \
  --offline-full \
  --base-url "${SOURCE_URL}" \
  --private-http-host "${SOURCE_HOST}" \
  --checkpoint /evidence/archive-source-checkpoint.json \
  --output /evidence/archive-baseline-000001.jsonl \
  --since 1970-01-01T00:00:00Z \
  --overlap-seconds 86400 \
  --session-limit 1000 \
  --timeout-seconds 60 \
  --max-elapsed-seconds 21600
sha256sum /evidence/archive-baseline-000001.jsonl
```

HTTP 503 means digest preparation is still bounded and in progress; HTTP 429
means the one-snapshot capacity is occupied. The exporter retries both with a
bounded backoff. HTTP 410 or an expired download ticket discards only the
unpublished attempt. It never advances the atomic checkpoint until every page,
record count and digest has been verified.

## Online deltas

After the baseline is sealed, point the same migration-only Service at the live
collector and remove the offline collector. Verify the Service endpoints and
NetworkPolicy before continuing. Use a new output name, omit `--since`, and do
not pass `--offline-full`:

```sh
set -eu
umask 077
SOURCE_HOST=cpa-session-archive-migration.cpa.svc.cluster.local
SOURCE_URL=http://${SOURCE_HOST}:8080
python3 ops/export-cpa-session-archive-delta.py \
  --collector-direct \
  --base-url "${SOURCE_URL}" \
  --private-http-host "${SOURCE_HOST}" \
  --checkpoint /evidence/archive-source-checkpoint.json \
  --output /evidence/archive-delta-000002.jsonl \
  --overlap-seconds 86400 \
  --session-limit 1000 \
  --timeout-seconds 60 \
  --max-elapsed-seconds 21600
```

Repeat with monotonically numbered output files. A stable ingest fence includes
late arrivals even when provider timestamps predate the overlap. Whole sessions
intentionally contain replayed records; target provenance removes duplicates.

## Target dry run, apply and replay

Mount one sealed JSONL read-only in the Token Center migration Job. Keep the
source name and overlap unchanged for all three invocations:

```sh
set -eu
export SESSION_ARCHIVE_FILE=/evidence/archive-delta-000002.jsonl
export SESSION_ARCHIVE_IMPORT_SOURCE=cpa-session-archive-production
export SESSION_ARCHIVE_OVERLAP_MS=86400000

SESSION_ARCHIVE_APPLY=false ops/import-cpa-session-archive.sh
SESSION_ARCHIVE_APPLY=true ops/import-cpa-session-archive.sh
SESSION_ARCHIVE_APPLY=true ops/import-cpa-session-archive.sh
```

The third command must report `imported: 0`; overlap records may report as
`replayed`. Preserve the JSONL, adjacent manifest, source checkpoint, three
import summaries and target checkpoint as one audit set. `--resume` is only for
recovering the exact same sealed output/manifest transition and performs no
source request.

## Failure and rollback

- Never edit a checkpoint, manifest or JSONL to bypass a refusal.
- On 401/403/invalid protocol, stop; those responses are not transient.
- On repeated 410, 429 or 503, retain evidence and investigate collector health,
  active snapshot age and digest queue statistics.
- Rolling back the migration means stopping the migration Job and restoring the
  migration-only Service selector. It does not change CPA traffic.
- Delete short-lived clone PVCs and migration Jobs only after retained evidence
  paths and ownership have been reviewed; never delete production archive data.
