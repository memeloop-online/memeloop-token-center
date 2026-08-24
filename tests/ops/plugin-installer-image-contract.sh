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
grep -Fq 'ARG GO_IMAGE=golang:1.26.7-bookworm' "${dockerfile}" \
  || fail "patched Cosign must use the fixed Go 1.26.7 toolchain"
grep -Fq 'https://codeload.github.com/sigstore/cosign/tar.gz/11926fa5bbbbde47e88fc006b625a17769b743b2' \
  "${dockerfile}" || fail "Cosign source is not fixed to the signed v3.1.3 tag commit"
grep -Fq 'sha256:3a718446bac51466efff6853639e1ca108b456ecbf07cd92938f548715d22d6b' \
  "${dockerfile}" || fail "Cosign source archive checksum is missing"
grep -Fq 'COPY packaging/cosign/v3.1.3-security.patch' "${dockerfile}" \
  || fail "reviewed Cosign security backport is not applied"
grep -Fq 'git apply --unidiff-zero --check /tmp/v3.1.3-security.patch' "${dockerfile}" \
  || fail "Cosign security backport is not preflighted against the pinned source"
grep -Fq 'GOTOOLCHAIN=local go mod verify' "${dockerfile}" \
  || fail "patched Cosign dependencies are not verified"
! grep -Fq 'github.com/sigstore/cosign/releases/download/' "${dockerfile}" \
  || fail "vulnerable upstream release binary must not enter the runtime"
! grep -Eq '^ARG[[:space:]]+COSIGN_(VERSION|SHA|DIGEST|COMMIT)' "${dockerfile}" \
  || fail "Cosign identity must not be caller-overridable"
grep -Fq 'ARG RUNTIME_IMAGE=gcr.io/distroless/base-nossl-debian13:nonroot' "${dockerfile}" \
  || fail "installer runtime must exclude the unused OpenSSL runtime"
grep -Fq 'COPY --from=builder /tmp/libgcc_s.so.1 /usr/local/lib/libgcc_s.so.1' \
  "${dockerfile}" || fail "installer runtime must receive only its required libgcc runtime"
grep -Fq 'ENV LD_LIBRARY_PATH=/usr/local/lib' "${dockerfile}" \
  || fail "installer runtime must resolve its copied libgcc"
grep -Fq 'USER 10001:10001' "${dockerfile}" \
  || fail "installer runtime must be non-root"
grep -Fq 'ENTRYPOINT ["/usr/local/bin/install-plugin-oci"]' "${dockerfile}" \
  || fail "installer runtime must use an exec-form entrypoint"
! grep -Fq '/usr/local/bin/cosign' "${repository_root}/Dockerfile" \
  || fail "Cosign must not enter the long-running service image"
! grep -Eq '^sigstore[[:space:]]*=' "${repository_root}/Cargo.toml" \
  || fail "Rust Sigstore must not remain in the product dependency graph"

docker build --pull \
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
test "$(printf '%s' "${version_json}" | jq -r '.gitVersion')" = 'v3.1.3-mtc.1' \
  || fail "runtime Cosign version is not exactly v3.1.3-mtc.1"
test "$(printf '%s' "${version_json}" | jq -r '.goVersion')" = 'go1.26.7' \
  || fail "runtime Cosign was not built with fixed Go 1.26.7"
docker run --rm "${image}" --help >/dev/null

echo "plugin installer image contract: passed"
