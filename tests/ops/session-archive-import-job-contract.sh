#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
job="$repository/ops/kubernetes/session-archive-import-job.yaml"

test -f "$job"
grep -Fq 'name: memeloop-token-center-session-archive-import' "$job"
test "$(grep -c 'namespace: memeloop-token-center-dev' "$job")" -eq 3
grep -Fq 'image: REPLACE_PRIVATE_REGISTRY/memeloop-token-center@sha256:REPLACE_DIGEST' "$job"
grep -Fq 'command: ["/usr/local/bin/import-cpa-session-archive"]' "$job"
grep -Fq 'automountServiceAccountToken: false' "$job"
grep -Fq 'readOnlyRootFilesystem: true' "$job"
grep -Fq 'allowPrivilegeEscalation: false' "$job"
grep -Fq 'runAsNonRoot: true' "$job"
grep -Fq 'runAsUser: 10001' "$job"
grep -Fq 'type: RuntimeDefault' "$job"
grep -Fq 'drop: ["ALL"]' "$job"
grep -Fq -- '- --plan-directory' "$job"
grep -Fq -- '- /plan' "$job"
grep -Fq -- '- --max-plan-bytes' "$job"
grep -Fq -- '- "1073741824"' "$job"
grep -Fq 'sizeLimit: 1200Mi' "$job"
grep -Fq 'mountPath: /source' "$job"
test "$(grep -c 'readOnly: true' "$job")" -ge 2
grep -Fq 'secretKeyRef:' "$job"
test "$(grep -c '^kind: NetworkPolicy$' "$job")" -eq 2
grep -Fq 'name: memeloop-token-center-session-archive-import-default-deny' "$job"
grep -Fq 'name: memeloop-token-center-session-archive-import-egress' "$job"
grep -Fq 'kubernetes.io/metadata.name: kube-system' "$job"
grep -Fq 'k8s-app: kube-dns' "$job"
grep -Fq 'port: 53' "$job"
grep -Fq 'port: 5432' "$job"
grep -Fq 'port: 9000' "$job"
test "$(grep -c 'kubernetes.io/metadata.name: memeloop-token-center-dev' "$job")" -eq 2
grep -Fq 'cnpg.io/cluster: memeloop-token-center-pg' "$job"
grep -Fq 'app.kubernetes.io/name: minio' "$job"
grep -Fq 'value: http://minio.memeloop-token-center-dev.svc.cluster.local:9000' "$job"
grep -A1 -F 'name: MTC_S3_ALLOW_HTTP' "$job" | grep -Fq 'value: "true"'
grep -Fq -- '- Ingress' "$job"
grep -Fq -- '- Egress' "$job"

if grep -Eq 'name: MTC_(KEY_PEPPER|SERVICE_TOKEN)' "$job"; then
  echo "session archive Job must not mount unused control-plane credentials" >&2
  exit 1
fi
if grep -Fq '0.0.0.0/0' "$job"; then
  echo "session archive Job must not allow unrestricted egress" >&2
  exit 1
fi

if grep -Eq '(mts_|mtc_|postgres://[^R]|password:[[:space:]]*[^R])' "$job"; then
  echo "session archive Job must not embed credential material" >&2
  exit 1
fi

binary="$repository/src/bin/import-cpa-session-archive.rs"
wrapper="$repository/ops/import-cpa-session-archive.sh"
grep -Fq 'Config::from_session_archive_import_env()' "$binary"
grep -Fq 'ensure_session_archive_import_schema()' "$binary"
grep -Fq 'SESSION_ARCHIVE_MAX_LINE_BYTES:=16777216' "$wrapper"
grep -Fq 'SESSION_ARCHIVE_MAX_LINE_BYTES must not exceed the 16 MiB importer hard limit' "$wrapper"
if grep -Fq '.migrate()' "$binary"; then
  echo "session archive binary must not run target database migrations" >&2
  exit 1
fi

# The checked-in template must remain dry-run by default.
if grep -Eq '^[[:space:]]+- --apply[[:space:]]*$' "$job"; then
  echo "session archive Job unexpectedly enables apply" >&2
  exit 1
fi
