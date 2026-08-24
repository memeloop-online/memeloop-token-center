#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
workflow_directory="$repository/.github/workflows"
workflow="$workflow_directory/ci.yml"
workflow_policy="$repository/web/scripts/verify-github-workflow-policy.mjs"
workflow_policy_fixtures="$repository/tests/ops/github-workflow-policy-fixtures.sh"
shared_contracts="$repository/ops/ci/run-release-source-contracts.sh"
dockerfile="$repository/Dockerfile"
importer_dockerfile="$repository/Dockerfile.importer"
plugin_installer_dockerfile="$repository/Dockerfile.plugin-installer"
importer_contract="$repository/tests/ops/importer-image-contract.sh"
bundle_preparer="$repository/ops/ci/prepare-cpamp-acceptance-bundle.sh"
cargo_config="$repository/.cargo/config.toml"
bundle_test_root=$(mktemp -d "${TMPDIR:-/tmp}/mtc-cpamp-bundle-contract.XXXXXX")
cleanup() {
  chmod -R u+w -- "$bundle_test_root" 2>/dev/null || true
  rm -rf -- "$bundle_test_root"
}
trap cleanup EXIT HUP INT TERM

test -f "$workflow"
test -f "$workflow_directory/memory-acceptance.yml"
test -f "$dockerfile"
test -f "$importer_dockerfile"
test -f "$plugin_installer_dockerfile"
test -f "$importer_contract"
test -x "$bundle_preparer"
grep -Fq '[ -f "$source_file" ] && [ ! -L "$source_file" ]' "$bundle_preparer"
test -x "$workflow_policy"
test -x "$workflow_policy_fixtures"
test -x "$shared_contracts"
test -f "$cargo_config"
test ! -e "$repository/.forgejo"
sh -n "$importer_contract" "$bundle_preparer"

bundle="$bundle_test_root/bundle"
mkdir -m 0700 -- "$bundle"
"$bundle_preparer" "$bundle"
mkdir -m 0700 -- "$bundle_test_root/real-target"
ln -s real-target "$bundle_test_root/target-link"
if "$bundle_preparer" "$bundle_test_root/target-link" >/dev/null 2>&1; then
  echo 'CPAMP acceptance bundle preparer accepted a symlink target' >&2
  exit 1
fi
ln -s "$bundle_test_root" "$bundle_test_root/parent-link"
if "$bundle_preparer" "$bundle_test_root/parent-link/escape" >/dev/null 2>&1; then
  echo 'CPAMP acceptance bundle preparer accepted a symlink parent path' >&2
  exit 1
fi
actual_bundle=$(CDPATH='' cd -- "$bundle" && find . -mindepth 1 -maxdepth 1 -type f -print \
  | sed 's#^./##' | sort)
expected_bundle=$(cat <<'EOF'
0001_initial.sql
0002_query_indexes.sql
0004_request_events.sql
0005_generation_jobs.sql
0018_model_price_tiers.sql
0019_session_archive_import.sql
0021_request_locators.sql
0022_budget_rollups.sql
0023_generation_daily_aggregates.sql
0024_request_stats_rollups.sql
0027_cpamp_source_digests.sql
cpamp-import-postgres-acceptance.sh
initial.sql
migrate-cpamp.sh
EOF
)
test "$actual_bundle" = "$expected_bundle"
cmp "$repository/tests/ops/cpamp-import-postgres-acceptance.sh" \
  "$bundle/cpamp-import-postgres-acceptance.sh"
cmp "$repository/ops/migrate-cpamp.sh" "$bundle/migrate-cpamp.sh"
cmp "$repository/tests/fixtures/cpamp/initial.sql" "$bundle/initial.sql"
for migration in 0001_initial 0002_query_indexes 0004_request_events \
  0005_generation_jobs 0018_model_price_tiers 0019_session_archive_import \
  0021_request_locators 0022_budget_rollups 0023_generation_daily_aggregates \
  0024_request_stats_rollups 0027_cpamp_source_digests; do
  source="$repository/migrations/postgres/$migration.sql"
  [ -f "$source" ] || source="$repository/migrations/common/$migration.sql"
  cmp "$source" "$bundle/$migration.sql"
done
test "$(stat -c '%a' "$bundle/cpamp-import-postgres-acceptance.sh")" = 555
test "$(stat -c '%a' "$bundle/migrate-cpamp.sh")" = 555
test "$(stat -c '%a' "$bundle")" = 555
test "$(find "$bundle" -maxdepth 1 -type f -name '*.sql' ! -perm 0444 -print -quit)" = ''

grep -Fq 'ARG NODE_IMAGE=node:24.18.0-bookworm-slim' "$dockerfile"
grep -Fq 'ARG RUST_IMAGE=rust:1.95.0-bookworm' "$dockerfile"
grep -Fq 'ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot' "$dockerfile"
grep -Fq 'ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot' "$plugin_installer_dockerfile"
! grep -Fq 'DEBIAN_MIRROR' "$dockerfile"
! grep -Fq 'DEBIAN_MIRROR' "$plugin_installer_dockerfile"
! grep -Fq 'mirrors.tuna.tsinghua.edu.cn' "$dockerfile"
! grep -Fq 'mirrors.tuna.tsinghua.edu.cn' "$plugin_installer_dockerfile"
grep -Fq 'ARG RUNTIME_IMAGE=alpine:3.23.5' "$importer_dockerfile"
grep -Fq 'apk add --no-cache ca-certificates nodejs postgresql-client sqlite' \
  "$importer_dockerfile"
! grep -Fq 'apt-get' "$importer_dockerfile"
! grep -Eq '^ARG RUNTIME_IMAGE=.*(debian|bookworm)' "$importer_dockerfile"
grep -Fq 'COPY .cargo/config.toml /build/.cargo/config.toml' "$dockerfile"
grep -Fq 'COPY vendor ./vendor' "$dockerfile"
grep -Fq \
  'JEMALLOC_SYS_WITH_MALLOC_CONF = { value = "abort_conf:true,background_thread:true,dirty_decay_ms:0,muzzy_decay_ms:0", force = true }' \
  "$cargo_config"

config_copy_line=$(grep -n -F 'COPY .cargo/config.toml /build/.cargo/config.toml' "$dockerfile" \
  | cut -d: -f1)
vendor_copy_line=$(grep -n -F 'COPY vendor ./vendor' "$dockerfile" | cut -d: -f1)
first_cargo_build_line=$(grep -n -F 'cargo build --locked' "$dockerfile" | head -n 1 | cut -d: -f1)
if [ "$config_copy_line" -ge "$first_cargo_build_line" ]; then
  echo '.cargo/config.toml must enter the Docker builder before dependency compilation' >&2
  exit 1
fi
if [ "$vendor_copy_line" -ge "$first_cargo_build_line" ]; then
  echo 'vendored path dependencies must enter the Docker builder before dependency compilation' >&2
  exit 1
fi
grep -Fq \
  'cargo build --locked --release --bin memeloop-token-center --bin import-cpa-session-archive' \
  "$dockerfile"
grep -Fq 'cargo clean --release --package memeloop-token-center' "$dockerfile"
grep -Fq 'COPY build.rs ./build.rs' "$dockerfile"

workflow_files=$(find "$workflow_directory" -maxdepth 1 -type f \
  \( -name '*.yml' -o -name '*.yaml' \) -print | sort)
if [ -z "$workflow_files" ]; then
  echo 'No GitHub Actions workflows were found' >&2
  exit 1
fi
uses_lines=$(find "$workflow_directory" -maxdepth 1 -type f \
  \( -name '*.yml' -o -name '*.yaml' \) \
  -exec grep -HnE '^[[:space:]]*(-[[:space:]]+)?uses:' {} +)
if [ -z "$uses_lines" ]; then
  echo 'No GitHub Actions uses entries were found' >&2
  exit 1
fi
if printf '%s\n' "$uses_lines" |
  grep -Ev 'uses:[[:space:]]+(\./[^[:space:]#]+|[^[:space:]#]+@[0-9a-fA-F]{40}[[:space:]]+#[[:space:]]+[^[:space:]]+)' \
    >/dev/null; then
  echo 'Every external action must use a full 40-hex commit SHA and retain a version comment' >&2
  printf '%s\n' "$uses_lines" |
    grep -Ev 'uses:[[:space:]]+(\./[^[:space:]#]+|[^[:space:]#]+@[0-9a-fA-F]{40}[[:space:]]+#[[:space:]]+[^[:space:]]+)' \
      >&2
  exit 1
fi

grep -Fq 'node-version: 24.18.0' "$workflow"
test "$(grep -c 'toolchain: 1.95.0' "$workflow")" -eq 4

node "$workflow_policy" "$workflow" "$repository"

checkout_count=$(printf '%s\n' "$uses_lines" | grep -c 'actions/checkout@')
persist_false_count=$(grep -Rhc 'persist-credentials: false' "$workflow_directory"/* |
  awk '{ total += $1 } END { print total + 0 }')
if [ "$checkout_count" -ne "$persist_false_count" ]; then
  echo 'Every checkout must disable persisted GitHub credentials' >&2
  exit 1
fi

grep -Fq 'repository-security:' "$workflow"
grep -Fq 'dependency-security:' "$workflow"
grep -Fq 'scanners: secret,misconfig' "$workflow"
grep -Fq 'command: check advisories bans licenses sources' "$workflow"
grep -Fq 'verifyDependencySecurity(workflow.jobs['"'"'dependency-security'"'"'])' "$workflow_policy"
grep -Fq '/--ignore(?:\s|=|$)/iu' "$workflow_policy"
grep -Fq 'memory-acceptance:' "$workflow"
grep -Fq 'uses: ./.github/workflows/memory-acceptance.yml' "$workflow"
grep -Fq 'workflow_call:' "$workflow_directory/memory-acceptance.yml"
grep -Fq -- '--profile acceptance' "$workflow_directory/memory-acceptance.yml"
grep -Fq -- '--gateway-limit-mib 256' "$workflow_directory/memory-acceptance.yml"

cargo_resolution_lines=$(grep -E \
  'cargo (build|clippy|test|run|tree)([[:space:]]|$)' "$workflow")
if printf '%s\n' "$cargo_resolution_lines" | grep -v -- '--locked' >/dev/null; then
  echo 'Every CI Cargo resolution command must honor Cargo.lock' >&2
  printf '%s\n' "$cargo_resolution_lines" | grep -v -- '--locked' >&2
  exit 1
fi

grep -Fq 'ops/ci/validate-release-inputs.sh' "$workflow"
grep -Fq 'ops/ci/run-release-source-contracts.sh' "$workflow"
grep -Fq 'tests/ops/github-workflow-policy-fixtures.sh' "$shared_contracts"
grep -Fq 'ghcr.io/${{ github.repository_owner }}/memeloop-token-center' "$workflow"
grep -Fq 'ghcr.io/${{ github.repository_owner }}/memeloop-token-center-importer' "$workflow"
grep -Fq 'ghcr.io/${{ github.repository_owner }}/memeloop-token-center-plugin-installer' "$workflow"
grep -Fq 'dockerfile: Dockerfile' "$workflow"
grep -Fq 'dockerfile: Dockerfile.importer' "$workflow"
grep -Fq 'dockerfile: Dockerfile.plugin-installer' "$workflow"
grep -Fq "if: github.event_name == 'push' && github.ref == 'refs/heads/master'" "$workflow"
grep -Fq 'MTC_BUILD_GIT_SHA_INPUT=${{ github.sha }}' "$workflow"
grep -Fq 'ARG MTC_BUILD_GIT_SHA_INPUT=unknown' "$dockerfile"
grep -Fq 'tags: ${{ matrix.image }}:sha-${{ github.sha }}' "$workflow"
if grep -Eq 'tags:.*:master|tags:.*:latest|^[[:space:]]+\$\{\{ matrix.image \}\}:(master|latest)$' "$workflow"; then
  echo 'Moving master/latest tags must not be release or deployment evidence' >&2
  exit 1
fi
grep -Fq 'needs:' "$workflow"
for required_gate in repository-security dependency-security web rust migration-smoke \
  api-contract packaging memory-acceptance; do
  grep -Fq -- "- $required_gate" "$workflow"
done
grep -Fq 'scanners: vuln' "$workflow"
grep -Fq 'image-ref: ${{ matrix.image }}@${{ steps.build.outputs.digest }}' "$workflow"
grep -Fq 'severity: HIGH,CRITICAL' "$workflow"
grep -Fq 'provenance: mode=max' "$workflow"
grep -Fq 'test "$GITHUB_REPOSITORY" = memeloop-online/memeloop-token-center' "$workflow"
grep -Fq 'go-containerregistry/releases/download/v0.21.9/go-containerregistry_Linux_x86_64.tar.gz' "$workflow"
grep -Fq '5c16d8ddb971cb1d5e6ed8b1e743da8224414eeba2c2762d8f1a61b2f095699e' "$workflow"
grep -Fq 'ops/ci/verify-buildkit-attestations.sh' "$workflow"
grep -Fq 'crane digest "$tagged_reference"' "$workflow"
grep -Fq -- '--arg source "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY"' "$workflow"
grep -Fq '.config.Labels["org.opencontainers.image.source"] == $source' "$workflow"
grep -Fq '.config.Labels["org.opencontainers.image.revision"] == $revision' "$workflow"
grep -Fq '${{ runner.temp }}/${{ matrix.cache_scope }}-attestations/' "$workflow"
grep -Fq 'sbom: true' "$workflow"
grep -Fq 'verify-ghcr-release:' "$workflow"
grep -Fq 'name: ghcr-release-${{ github.sha }}' "$workflow"
grep -Fq 'length == 3 and' "$workflow"
# These are intentionally literal GitHub/JQ expressions in the audited workflow.
grep -Fq 'prefix="ghcr.io/${GITHUB_REPOSITORY_OWNER}"' "$workflow"
grep -Fq '((map(.image) | sort) == $expected_images)' "$workflow"
# shellcheck disable=SC2016
grep -Fq 'DIGEST: ${{ steps.build.outputs.digest }}' "$workflow"
# shellcheck disable=SC2016
grep -Fq 'name: image-digest-${{ matrix.cache_scope }}-${{ github.sha }}' "$workflow"
# shellcheck disable=SC2016
grep -Fq 'reference: ($image + "@" + $digest)' "$workflow"
grep -Fq 'if-no-files-found: error' "$workflow"

grep -Fq 'SQLite migration and replay smoke test' "$workflow"
grep -Fq 'PostgreSQL migration and replay smoke test' "$workflow"
grep -Fq 'Exercise CPAMP initial, overlap, incremental, and replay imports' "$workflow"
grep -Fq 'acceptance=$(mktemp -d "$RUNNER_TEMP/cpamp-acceptance-$run_id.XXXXXX")' "$workflow"
grep -Fq 'ops/ci/prepare-cpamp-acceptance-bundle.sh "$acceptance"' "$workflow"
grep -Fq -- '--env ACCEPTANCE_WORK_ROOT=/acceptance' "$workflow"
grep -Fq -- '--volume "$acceptance:/acceptance:ro"' "$workflow"
grep -Fq '/acceptance/cpamp-import-postgres-acceptance.sh' "$workflow"
grep -Fq 'rm -rf -- "$acceptance"' "$workflow"
if grep -Fq 'cpamp-import-postgres-acceptance.sh:/acceptance.sh:ro' "$workflow"; then
  echo 'CPAMP acceptance must mount the complete reviewed bundle, not one script' >&2
  exit 1
fi
grep -Fq 'test ! -e /work' "$importer_contract"
grep -Fq 'cargo fmt --all -- --check' "$workflow"
grep -Fq 'cargo clippy --locked --all-targets --all-features -- -D warnings' "$workflow"
grep -Fq 'cargo test --locked --all-targets --all-features' "$workflow"
grep -Fq "grep -Eq '^mig_[0-9a-f]{24}$'" "$workflow"
grep -Fq "printf 'CREATE SCHEMA :\"schema\";\\n'" "$workflow"
grep -Fq "printf 'DROP SCHEMA IF EXISTS :\"schema\" CASCADE;\\n'" "$workflow"
test "$(grep -c '| psql -X --no-psqlrc -v ON_ERROR_STOP=1' "$workflow")" -ge 2
if grep -Eq -- "-c ['\"](CREATE|DROP) SCHEMA" "$workflow"; then
  echo 'psql variables must be expanded from stdin, never sent literally with -c' >&2
  exit 1
fi
for importer_binary in migrate-cpamp audit-cpa-migration \
  attach-legacy-cpa-credentials import-cpa-upstreams \
  generate-source-identity-key; do
  grep -Fqx \
    "docker cp \"\$container_id:/usr/local/bin/$importer_binary\" \"\$workspace/image-$importer_binary\"" \
    "$importer_contract"
done
test "$(grep -c '^docker cp "\$container_id:/usr/local/bin/' "$importer_contract")" -eq 5
grep -Fq 'npm audit --audit-level=high' "$workflow"
grep -Fq 'npm run test:e2e' "$workflow"

echo 'Release workflow and Docker packaging contract OK'
