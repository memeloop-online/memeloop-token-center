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
if printf '%s\n' "$uses_lines" \
  | grep -Ev 'uses:[[:space:]]+[^[:space:]#]+@[0-9a-fA-F]{40}[[:space:]]+#[[:space:]]+[^[:space:]]+' >/dev/null; then
  echo 'Every action in every GitHub workflow must use a full 40-hex commit SHA and retain a version comment' >&2
  printf '%s\n' "$uses_lines" \
    | grep -Ev 'uses:[[:space:]]+[^[:space:]#]+@[0-9a-fA-F]{40}[[:space:]]+#[[:space:]]+[^[:space:]]+' >&2
  exit 1
fi

grep -Fq 'node-version: 24.18.0' "$workflow"
test "$(grep -c 'toolchain: 1.95.0' "$workflow")" -eq 3

cargo_resolution_lines=$(grep -E \
  'cargo (build|clippy|test|run|tree)([[:space:]]|$)' "$workflow")
if printf '%s\n' "$cargo_resolution_lines" | grep -v -- '--locked' >/dev/null; then
  echo 'Every CI Cargo resolution command must honor Cargo.lock' >&2
  printf '%s\n' "$cargo_resolution_lines" | grep -v -- '--locked' >&2
  exit 1
fi

grep -Fq 'ghcr.io/linonetwo/memeloop-token-center' "$workflow"
grep -Fq 'ghcr.io/linonetwo/memeloop-token-center-importer' "$workflow"
grep -Fq 'dockerfile: Dockerfile' "$workflow"
grep -Fq 'dockerfile: Dockerfile.importer' "$workflow"
grep -Fq 'MTC_BUILD_GIT_SHA_INPUT=${{ github.sha }}' "$workflow"
grep -Fq 'ARG MTC_BUILD_GIT_SHA_INPUT=unknown' "$dockerfile"
# These are intentionally literal GitHub/JQ expressions in the audited workflow.
# shellcheck disable=SC2016
grep -Fq 'DIGEST: ${{ steps.build.outputs.digest }}' "$workflow"
# shellcheck disable=SC2016
grep -Fq 'name: image-digest-${{ matrix.cache_scope }}-${{ github.sha }}' "$workflow"
# shellcheck disable=SC2016
grep -Fq 'reference: ($image + "@" + $digest)' "$workflow"
grep -Fq 'if-no-files-found: error' "$workflow"

echo 'Release workflow and Docker packaging contract OK'
