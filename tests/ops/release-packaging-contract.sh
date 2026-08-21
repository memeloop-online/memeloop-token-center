#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
workflow_directory="$repository/.github/workflows"
workflow="$workflow_directory/ci.yml"
dockerfile="$repository/Dockerfile"
cargo_config="$repository/.cargo/config.toml"

test -f "$workflow"
test -f "$workflow_directory/memory-acceptance.yml"
test -f "$dockerfile"
test -f "$cargo_config"
test -f "$repository/.forgejo/workflows/harbor-release.yml"
test ! -e "$repository/.forgejo/workflows/build.yaml"

grep -Fq 'ARG NODE_IMAGE=node:24.18.0-bookworm-slim' "$dockerfile"
grep -Fq 'ARG RUST_IMAGE=rust:1.95.0-bookworm' "$dockerfile"
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

# Default to read-only and grant package publication only to the master release job.
test "$(grep -c '^[[:space:]]*contents: read$' "$workflow")" -ge 2
test "$(grep -c '^[[:space:]]*packages: write$' "$workflow")" -eq 1
test "$(grep -c '^[[:space:]]*packages: read$' "$workflow")" -eq 1
test "$(grep -c '^[[:space:]]*contents: write$' "$workflow" || true)" -eq 0
test "$(grep -c '^[[:space:]]*id-token: write$' "$workflow" || true)" -eq 0

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
grep -Fq 'uses: rustsec/audit-check@69366f33c96575abad1ee0dba8212993eecbe998 # v2.0.0' "$workflow"
if grep -A4 -F 'uses: rustsec/audit-check@' "$workflow" | grep -Eq 'ignore:|--ignore'; then
  echo 'RustSec CI must not suppress any advisory' >&2
  exit 1
fi
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
grep -Fq 'ghcr.io/linonetwo/memeloop-token-center' "$workflow"
grep -Fq 'ghcr.io/linonetwo/memeloop-token-center-importer' "$workflow"
grep -Fq 'ghcr.io/linonetwo/memeloop-token-center-plugin-installer' "$workflow"
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
grep -Fq 'sbom: true' "$workflow"
grep -Fq -- '--format '"'"'{{json .SBOM}}'"'"'' "$workflow"
grep -Fq -- '--format '"'"'{{json .Provenance}}'"'"'' "$workflow"
grep -Fq 'verify-ghcr-release:' "$workflow"
grep -Fq 'name: ghcr-release-${{ github.sha }}' "$workflow"
grep -Fq 'length == 3 and' "$workflow"
# These are intentionally literal GitHub/JQ expressions in the audited workflow.
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
grep -Fq 'tests/ops/cpamp-import-postgres-acceptance.sh:/acceptance.sh:ro' "$workflow"
grep -Fq 'cargo fmt --all -- --check' "$workflow"
grep -Fq 'cargo clippy --locked --all-targets --all-features -- -D warnings' "$workflow"
grep -Fq 'cargo test --locked --all-targets --all-features' "$workflow"
grep -Fq 'npm audit --audit-level=high' "$workflow"
grep -Fq 'npm run test:e2e' "$workflow"

echo 'Release workflow and Docker packaging contract OK'
