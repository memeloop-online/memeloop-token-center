#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
workspace=$(mktemp -d "${TMPDIR:-/tmp}/mtc-importer-contract.XXXXXX")
image=${IMPORTER_IMAGE:-memeloop-token-center-importer-contract:$$}
created_image=false
container_id=
fixture_volume=

cleanup() {
  if [ -n "$container_id" ]; then
    docker container rm "$container_id" >/dev/null 2>&1 || true
  fi
  if [ "$created_image" = true ]; then
    docker image rm "$image" >/dev/null 2>&1 || true
  fi
  if [ -n "$fixture_volume" ]; then
    docker volume rm "$fixture_volume" >/dev/null 2>&1 || true
  fi
  rm -rf -- "$workspace"
}
trap cleanup EXIT HUP INT TERM

if [ -z "${IMPORTER_IMAGE:-}" ]; then
  set -- docker build --file "$repository/Dockerfile.importer" --tag "$image"
  if [ -n "${IMPORTER_RUNTIME_IMAGE:-}" ]; then
    set -- "$@" --build-arg "RUNTIME_IMAGE=$IMPORTER_RUNTIME_IMAGE"
  fi
  "$@" "$repository"
  created_image=true
fi

test "$(docker image inspect "$image" --format '{{.Config.User}}')" = "10001:10001"
test "$(docker image inspect "$image" --format '{{json .Config.Entrypoint}}')" = \
  '["/usr/local/bin/migrate-cpamp"]'

docker run --rm \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=8m \
  --security-opt no-new-privileges \
  --cap-drop ALL \
  --entrypoint /bin/sh \
  "$image" -ec '
    test "$(id -u)" = 10001
    test "$(id -g)" = 10001
    test -x /usr/local/bin/migrate-cpamp
    test -x /usr/local/bin/audit-cpa-migration
    test -x /usr/local/bin/attach-legacy-cpa-credentials
    test -x /usr/local/bin/import-cpa-upstreams
    test -x /usr/local/bin/generate-source-identity-key
    test ! -w /usr/local/bin/migrate-cpamp
    test ! -w /usr/local/bin/audit-cpa-migration
    test ! -w /usr/local/bin/attach-legacy-cpa-credentials
    test ! -w /usr/local/bin/import-cpa-upstreams
    test ! -w /usr/local/bin/generate-source-identity-key
    test "$(stat -c %a /usr/local/bin/migrate-cpamp)" = 555
    test "$(stat -c %a /usr/local/bin/audit-cpa-migration)" = 555
    test "$(stat -c %a /usr/local/bin/attach-legacy-cpa-credentials)" = 555
    test "$(stat -c %a /usr/local/bin/import-cpa-upstreams)" = 555
    test "$(stat -c %a /usr/local/bin/generate-source-identity-key)" = 555
    command -v psql >/dev/null
    command -v node >/dev/null
    command -v sqlite3 >/dev/null
    node --version | grep -Eq "^v24\\."
    psql --version | grep -Eq "^psql \\(PostgreSQL\\) 18\\."
    test ! -e /tests
    test ! -e /source
    test ! -e /work
  '

docker run --rm \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=8m \
  --security-opt no-new-privileges \
  --cap-drop ALL \
  --entrypoint /usr/local/bin/attach-legacy-cpa-credentials \
  "$image" --help >"$workspace/legacy-help.txt"
grep -Fq 'dry-run by default' "$workspace/legacy-help.txt"
if grep -Eq -- '--credential([ =]|$)' "$workspace/legacy-help.txt"; then
  echo 'legacy importer must not accept a plaintext credential argument' >&2
  exit 1
fi

docker run --rm \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=8m \
  --security-opt no-new-privileges \
  --cap-drop ALL \
  --entrypoint /usr/local/bin/import-cpa-upstreams \
  "$image" --help >"$workspace/cpa-upstream-help.txt"
grep -Fq 'dry-run by default' "$workspace/cpa-upstream-help.txt"
if grep -Eq -- '--(credential|api-key|service-token)([ =]|$)' \
  "$workspace/cpa-upstream-help.txt"; then
  echo 'CPA upstream importer must not accept a plaintext secret argument' >&2
  exit 1
fi
if grep -Eqi -- 'bridge|subscription-accounts' "$workspace/cpa-upstream-help.txt"; then
  echo 'CPA upstream importer must not expose retired relay options' >&2
  exit 1
fi

cp -R "$repository/tests/fixtures/cpa-upstreams/supported" \
  "$workspace/cpa-upstream-source"
# These are synthetic fixtures. The privileged preparation container copies them
# into a private volume and applies the production ownership/modes before the
# non-root importer sees them.
find "$workspace/cpa-upstream-source" -type d -exec chmod 0755 {} +
find "$workspace/cpa-upstream-source" -type f -exec chmod 0644 {} +
fixture_volume="mtc-cpa-upstream-fixture-$$"
docker volume create "$fixture_volume" >/dev/null
docker run --rm \
  --user 0:0 \
  --security-opt no-new-privileges \
  --cap-drop ALL \
  --cap-add CHOWN \
  --volume "$workspace/cpa-upstream-source:/fixture:ro" \
  --volume "$fixture_volume:/source" \
  --entrypoint /bin/sh \
  "$image" -ec '
    cp -R /fixture/. /source/
    find /source -type d -exec chmod 0700 {} +
    find /source -type f -exec chmod 0600 {} +
    chown -R 10001:10001 /source
  '
docker run --rm \
  --user 10001:10001 \
  --read-only \
  --security-opt no-new-privileges \
  --cap-drop ALL \
  --volume "$fixture_volume:/source" \
  --entrypoint /usr/local/bin/generate-source-identity-key \
  "$image" /source/source-identity.key
docker run --rm \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=8m \
  --security-opt no-new-privileges \
  --cap-drop ALL \
  --volume "$fixture_volume:/source:ro" \
  --entrypoint /usr/local/bin/import-cpa-upstreams \
  "$image" \
  --config /source/config.yaml \
  --auth-dir /source/auth \
  --source-identity-key-file /source/source-identity.key \
  >"$workspace/cpa-upstream-dry-run.json"
grep -Fq '"mode":"dry-run"' "$workspace/cpa-upstream-dry-run.json"
grep -Fq '"api_account_count":6' "$workspace/cpa-upstream-dry-run.json"
grep -Fq '"native_reauthorization_required_count":2' \
  "$workspace/cpa-upstream-dry-run.json"
if grep -Eq 'fixture-only-|Fixture(Copilot|Cursor)Handle' \
  "$workspace/cpa-upstream-dry-run.json"; then
  echo 'CPA upstream dry-run leaked fixture credential material' >&2
  exit 1
fi

key_staging_job="$repository/ops/kubernetes/cpa-upstream-import-dry-run-job.yaml"
grep -Fq 'automountServiceAccountToken: false' "$key_staging_job"
grep -A1 -F 'imagePullSecrets:' "$key_staging_job" \
  | grep -Fq 'name: REPLACE_IMAGE_PULL_SECRET'
test "$(grep -c 'image: REPLACE_PRIVATE_REGISTRY/memeloop-token-center-importer@sha256:REPLACE_DIGEST' "$key_staging_job")" -eq 2
grep -Fq 'name: stage-source-identity-key' "$key_staging_job"
grep -Fq 'cp -- /secret-source/source-identity.key /key-runtime/source-identity.key' "$key_staging_job"
grep -Fq 'chown 10001:10001 /key-runtime/source-identity.key' "$key_staging_job"
grep -Fq 'chmod 0600 /key-runtime/source-identity.key' "$key_staging_job"
chmod_line=$(grep -nF 'chmod 0600 /key-runtime/source-identity.key' \
  "$key_staging_job" | cut -d: -f1)
chown_line=$(grep -nF 'chown 10001:10001 /key-runtime/source-identity.key' \
  "$key_staging_job" | cut -d: -f1)
test -n "$chmod_line"
test -n "$chown_line"
test "$chmod_line" -lt "$chown_line"
grep -Fq 'test -f /key-runtime/source-identity.key' "$key_staging_job"
grep -Fq '10001:10001:600:1' "$key_staging_job"
grep -Fq 'defaultMode: 0400' "$key_staging_job"
grep -Fq 'medium: Memory' "$key_staging_job"
grep -Fq 'sizeLimit: 1Mi' "$key_staging_job"
test "$(grep -c 'mountPath: /secret-source' "$key_staging_job")" -eq 1
test "$(grep -c 'mountPath: /key-runtime' "$key_staging_job")" -eq 2
grep -A2 -F 'mountPath: /key-runtime' "$key_staging_job" | grep -Fq 'readOnly: true'
grep -A1 -F -- '- --source-identity-key-file' "$key_staging_job" \
  | grep -Fq -- '- /key-runtime/source-identity.key'
if grep -Eq '^[[:space:]]*-[[:space:]]*--apply[[:space:]]*$' "$key_staging_job"; then
  echo 'checked-in CPA upstream import Job must remain a dry-run' >&2
  exit 1
fi

if docker run --rm \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,size=8m \
  --security-opt no-new-privileges \
  --cap-drop ALL \
  "$image" >"$workspace/default.out" 2>"$workspace/default.err"; then
  echo 'default CPAMP importer unexpectedly succeeded without required inputs' >&2
  exit 1
fi
grep -Fq 'PGHOST is required' "$workspace/default.err"

container_id=$(docker create --entrypoint /bin/true "$image")
docker export "$container_id" >"$workspace/rootfs.tar"
docker cp "$container_id:/usr/local/bin/migrate-cpamp" "$workspace/image-migrate-cpamp"
docker cp "$container_id:/usr/local/bin/audit-cpa-migration" "$workspace/image-audit-cpa-migration"
docker cp "$container_id:/usr/local/bin/attach-legacy-cpa-credentials" "$workspace/image-attach-legacy-cpa-credentials"
docker cp "$container_id:/usr/local/bin/import-cpa-upstreams" "$workspace/image-import-cpa-upstreams"
docker cp "$container_id:/usr/local/bin/generate-source-identity-key" "$workspace/image-generate-source-identity-key"
cmp "$repository/ops/migrate-cpamp.sh" "$workspace/image-migrate-cpamp"
cmp "$repository/ops/audit-cpa-migration.sh" "$workspace/image-audit-cpa-migration"
node --check "$workspace/image-attach-legacy-cpa-credentials"
node --check "$workspace/image-import-cpa-upstreams"
node --check "$workspace/image-generate-source-identity-key"
# Stream only regular-file payloads from the image tar. Do not extract the
# rootfs: absolute compatibility symlinks such as /var/run must never resolve
# into the host while a packaging test inspects an image.
if tar -xOf "$workspace/rootfs.tar" 2>/dev/null \
  | grep -a -E -m1 \
    'fixture-only-cpa-(linux-codex|claude-code)-key|fixture-service-token' \
    >"$workspace/forbidden-material.txt"; then
  echo 'importer image contains test credential or token material' >&2
  exit 1
fi

job="$repository/ops/kubernetes/legacy-credential-import-job.yaml"
grep -Fq 'image: REPLACE_PRIVATE_REGISTRY/memeloop-token-center-importer@sha256:REPLACE_DIGEST' "$job"
grep -Fq 'command: ["/usr/local/bin/attach-legacy-cpa-credentials"]' "$job"
grep -Fq 'automountServiceAccountToken: false' "$job"
grep -A1 -F 'imagePullSecrets:' "$job" | grep -Fq 'name: REPLACE_IMAGE_PULL_SECRET'
grep -Fq 'readOnlyRootFilesystem: true' "$job"
grep -Fq 'allowPrivilegeEscalation: false' "$job"
grep -Fq 'fsGroup: 10001' "$job"
grep -Fq 'runAsUser: 10001' "$job"
grep -Fq 'name: PGPASSFILE' "$job"
grep -A1 -F 'name: PGHOST' "$job" | grep -Fq 'value: REPLACE_TARGET_POSTGRES_HOST'
grep -A1 -F 'name: PGDATABASE' "$job" | grep -Fq 'value: REPLACE_TARGET_DATABASE'
if grep -Eq '^[[:space:]]*-[[:space:]]*--apply[[:space:]]*$' "$job"; then
  echo 'checked-in legacy credential Job must remain a dry-run' >&2
  exit 1
fi
if grep -Eq '^[[:space:]]*-[[:space:]]*name:[[:space:]]*PGPASSWORD[[:space:]]*$' "$job"; then
  echo 'database password must not be exposed through the Job environment' >&2
  exit 1
fi

cpamp_job="$repository/ops/kubernetes/cpamp-import-job.yaml"
grep -Fq 'image: REPLACE_PRIVATE_REGISTRY/memeloop-token-center-importer@sha256:REPLACE_DIGEST' "$cpamp_job"
grep -Fq 'automountServiceAccountToken: false' "$cpamp_job"
grep -A1 -F 'imagePullSecrets:' "$cpamp_job" | grep -Fq 'name: REPLACE_IMAGE_PULL_SECRET'
grep -Fq 'readOnlyRootFilesystem: true' "$cpamp_job"
grep -Fq 'allowPrivilegeEscalation: false' "$cpamp_job"
grep -Fq 'runAsUser: 10001' "$cpamp_job"
grep -Fq 'fsGroup: 10001' "$cpamp_job"
grep -Fq 'readOnly: true' "$cpamp_job"
grep -Fq 'name: PGPASSFILE' "$cpamp_job"
grep -Fq 'name: CPAMP_RESET_IMPORT' "$cpamp_job"
grep -A1 -F 'name: CPAMP_RESET_IMPORT' "$cpamp_job" | grep -Fq 'value: "false"'
grep -A1 -F 'name: PGHOST' "$cpamp_job" | grep -Fq 'value: REPLACE_TARGET_POSTGRES_HOST'
grep -A1 -F 'name: PGDATABASE' "$cpamp_job" | grep -Fq 'value: REPLACE_TARGET_DATABASE'
grep -A1 -F 'name: CPAMP_IMPORT_SOURCE' "$cpamp_job" | grep -Fq 'value: REPLACE_CPAMP_IMPORT_SOURCE'
for import_job in "$job" "$cpamp_job"; do
  grep -Fq 'initContainers:' "$import_job"
  grep -Fq 'name: prepare-database-credentials' "$import_job"
  grep -Fq 'cp /secrets/database-source/pgpass /credentials/pgpass' "$import_job"
  grep -Fq 'chmod 0600 /credentials/pgpass' "$import_job"
  grep -Fq '"10001:10001:600"' "$import_job"
  grep -A1 -F 'name: PGPASSFILE' "$import_job" | grep -Fq 'value: /credentials/pgpass'
  grep -Fq 'name: database-secret' "$import_job"
  grep -Fq 'name: database-credentials' "$import_job"
  grep -Fq 'medium: Memory' "$import_job"
  ! grep -Fq 'value: /secrets/database/pgpass' "$import_job"
done
if grep -Fq 'memeloop_token_center_dogfood' "$job" "$cpamp_job"; then
  echo 'checked-in import Jobs must not pin the retired dogfood database name' >&2
  exit 1
fi
if grep -Eq '^[[:space:]]*-[[:space:]]*name:[[:space:]]*PGPASSWORD[[:space:]]*$' "$job" "$cpamp_job"; then
  echo 'database passwords must not be exposed through import Job environments' >&2
  exit 1
fi
if grep -Eq 'image:[[:space:]]+[^[:space:]]+@sha256:[0-9a-f]{64}' "$cpamp_job"; then
  echo 'checked-in CPAMP Job must use a release-time digest placeholder' >&2
  exit 1
fi

echo 'Importer image and import Job contracts OK'
