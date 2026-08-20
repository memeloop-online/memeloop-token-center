#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
dockerfile="${repository_root}/Dockerfile.plugin-installer"
image="mtc-plugin-installer-contract:$$"

fail() {
  echo "plugin installer image contract: $*" >&2
  exit 1
}

test -f "${dockerfile}" || fail "Dockerfile.plugin-installer is missing"
grep -Fq 'https://github.com/sigstore/cosign/releases/download/v3.1.3/' "${dockerfile}" \
  || fail "Cosign release is not fixed to v3.1.3"
grep -Fq '4629c757b7618056f8ddd7e2625ae9fdd94c0372a65049520bc7d9df9efc7f71' \
  "${dockerfile}" || fail "official linux-amd64 Cosign digest is missing"
grep -Fq 'c5d324e091826b0d7a78eb16fef316450b4eb9aaec045611c08ba06f5e73220a' \
  "${dockerfile}" || fail "official linux-arm64 Cosign digest is missing"
! grep -Eq '^ARG[[:space:]]+COSIGN_(VERSION|SHA|DIGEST)' "${dockerfile}" \
  || fail "Cosign version and digests must not be caller-overridable build arguments"
grep -Fq 'sha256sum --check --strict' "${dockerfile}" \
  || fail "downloaded Cosign asset is not verified"
grep -Fq 'USER 10001:10001' "${dockerfile}" \
  || fail "installer runtime must be non-root"
grep -Fq 'ENTRYPOINT ["/usr/local/bin/install-plugin-oci"]' "${dockerfile}" \
  || fail "installer runtime must use an exec-form entrypoint"
! grep -Fq '/usr/local/bin/cosign' "${repository_root}/Dockerfile" \
  || fail "Cosign must not enter the long-running service image"
! grep -Eq '^sigstore[[:space:]]*=' "${repository_root}/Cargo.toml" \
  || fail "Rust Sigstore must not remain in the product dependency graph"

case "$(docker info --format '{{.Architecture}}')" in
  amd64 | x86_64) target_arch='amd64' ;;
  arm64 | aarch64) target_arch='arm64' ;;
  *) fail "Docker daemon architecture must be amd64 or arm64" ;;
esac

docker build --pull \
  --build-arg "TARGETARCH=${target_arch}" \
  --file "${dockerfile}" \
  --tag "${image}" \
  "${repository_root}"

cleanup() {
  docker image rm --force "${image}" >/dev/null 2>&1 || true
}
trap cleanup EXIT HUP INT TERM

test "$(docker image inspect --format '{{.Config.User}}' "${image}")" = '10001:10001' \
  || fail "final image user is not 10001:10001"
test "$(docker image inspect --format '{{json .Config.Entrypoint}}' "${image}")" = \
  '["/usr/local/bin/install-plugin-oci"]' \
  || fail "final image entrypoint changed"

version_json="$(docker run --rm \
  --entrypoint /usr/local/bin/cosign \
  "${image}" version --json)"
test "$(printf '%s' "${version_json}" | jq -r '.gitVersion')" = 'v3.1.3' \
  || fail "runtime Cosign version is not exactly v3.1.3"
docker run --rm "${image}" --help >/dev/null

echo "plugin installer image contract: passed"
