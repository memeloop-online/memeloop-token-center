#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
workflow="$repository/.forgejo/workflows/harbor-release.yml"
publisher="$repository/ops/ci/publish-harbor-images.sh"
attestation_verifier="$repository/ops/ci/verify-buildkit-attestations.sh"
attestation_fixtures="$repository/tests/ops/forgejo-attestation-fixtures.sh"
validator="$repository/ops/ci/validate-release-inputs.sh"
shared_contracts="$repository/ops/ci/run-release-source-contracts.sh"
runbook="$repository/docs/operations/temporary-forgejo-harbor-release.md"

fail() {
  echo "Forgejo Harbor release contract: $*" >&2
  exit 1
}

test -f "$workflow"
test -x "$publisher"
test -x "$attestation_verifier"
test -x "$attestation_fixtures"
test -x "$validator"
test -x "$shared_contracts"
test -f "$runbook"
test ! -e "$repository/.forgejo/workflows/build.yaml"

grep -Fq 'name: temporary-harbor-release' "$workflow"
grep -Fq 'branches: [master]' "$workflow"
if grep -Eq '^[[:space:]]+(pull_request|pull_request_target|workflow_dispatch):' "$workflow"; then
  fail "release workflow must only run for trusted master pushes"
fi
grep -Fq 'contents: read' "$workflow"
if grep -Eq '^[[:space:]]+(contents|packages|actions|id-token): write' "$workflow"; then
  fail "workflow permissions must remain read-only"
fi

grep -Fq 'runs-on: mtc-quality-pod' "$workflow"
grep -Fq 'runs-on: mtc-release-rootless' "$workflow"
grep -Fq 'needs: [quality-gates]' "$workflow"
grep -Fq "if: forgejo.event_name == 'push' && forgejo.ref == 'refs/heads/master'" "$workflow"
grep -Fq 'test ! -S /var/run/docker.sock' "$workflow"
if grep -Eq '^[[:space:]]+(container|services):' "$workflow"; then
  fail "runner-Pod sidecars must not be replaced by Actions containers/services"
fi
if grep -Eq '(^|[[:space:]])docker[[:space:]]+(build|push|run|login)|/run/containerd/containerd.sock:' "$workflow"; then
  fail "workflow must not depend on Docker, DinD, or a mounted host container socket"
fi
if grep -Eqi '^[[:space:]]*(privileged|hostpath|hostnetwork|hostpid|hostipc):' "$workflow"; then
  fail "workflow may not opt into host or privileged access"
fi

grep -Fq 'test "$FORGEJO_SERVER_URL" = https://git.k3s.onetwo.website' "$workflow"
grep -Fq 'test "$FORGEJO_REPOSITORY" = mtc-ci/memeloop-token-center' "$workflow"
grep -Fq 'HARBOR_REPOSITORY_PREFIX: harbor.k3s.onetwo.website/mtc-ci' "$workflow"
grep -Fq 'BUILDKIT_HOST: ${{ vars.MTC_ROOTLESS_BUILDKIT_HOST }}' "$workflow"

uses_lines=$(grep -nE '^[[:space:]]*(-[[:space:]]+)?uses:' "$workflow")
test -n "$uses_lines"
if printf '%s\n' "$uses_lines" |
  grep -Ev 'uses:[[:space:]]+https://[^[:space:]#]+@[0-9a-fA-F]{40}[[:space:]]+#[[:space:]]+[^[:space:]]+' \
    >/dev/null; then
  printf '%s\n' "$uses_lines" >&2
  fail "every external action must use a fully qualified URL and full commit SHA"
fi
checkout_count=$(printf '%s\n' "$uses_lines" | grep -c '/actions/checkout@')
persist_count=$(grep -c 'persist-credentials: false' "$workflow")
test "$checkout_count" -eq "$persist_count" || fail "every checkout must drop persisted credentials"

actual_secrets=$(grep -oE 'secrets\.[A-Z0-9_]+' "$workflow" | sed 's/^secrets\.//' | sort -u)
expected_secrets=$(printf '%s\n' COSIGN_PASSWORD COSIGN_PRIVATE_KEY COSIGN_PUBLIC_KEY \
  HARBOR_PASSWORD HARBOR_USERNAME | sort)
test "$actual_secrets" = "$expected_secrets" || fail "unexpected or missing Forgejo secret reference"
if grep -Eq 'build-arg:[^[:space:]]*(PASSWORD|PRIVATE_KEY|TOKEN)|--(build-arg|opt)[^\n]*secrets\.' \
  "$workflow" "$publisher"; then
  fail "secret material must never enter image build arguments"
fi
publish_block=$(sed -n \
  '/- name: Build, scan, attest, sign, and verify immutable Harbor images/,/- name: Retain the digest-only release evidence/p' \
  "$workflow")
test "$(printf '%s\n' "$publish_block" | grep -c 'secrets\.' || true)" -eq 5 || \
  fail "all five release secrets must be scoped to the publish shell step"
job_env_block=$(sed -n '/^  release-harbor:/,/^[[:space:]]*steps:/p' "$workflow")
if printf '%s\n' "$job_env_block" | grep -q 'secrets\.'; then
  fail "release secrets must not be injected at job scope"
fi
checkout_block=$(sed -n \
  '/- name: Check out the exact gated revision/,/- name: Build, scan, attest, sign, and verify immutable Harbor images/p' \
  "$workflow")
if printf '%s\n' "$checkout_block" | grep -q 'secrets\.'; then
  fail "checkout must receive no Harbor or Cosign secret"
fi
artifact_block=$(sed -n '/- name: Retain the digest-only release evidence/,$p' "$workflow")
if printf '%s\n' "$artifact_block" | grep -q 'secrets\.'; then
  fail "artifact upload must receive no Harbor or Cosign secret"
fi
printf '%s\n' "$artifact_block" | grep -Fq 'retention-days: 7' || \
  fail "secret-free release evidence retention must remain short"

for required in \
  'cargo fmt --all -- --check' \
  'cargo clippy --locked --all-targets --all-features -- -D warnings' \
  'cargo test --locked --all-targets --all-features' \
  'cargo test --locked --test cucumber --all-features' \
  'npm --prefix web run test:localization' \
  'npm run test:e2e' \
  'SQLite migration and replay gate' \
  'PostgreSQL migration and replay gate' \
  'cpamp-import-postgres-acceptance.sh' \
  'tests/ops/test-openapi-contract.py' \
  'tests/ops/check-openapi-contract.py' \
  'tests/ops/helm-packaging-contract.sh' \
  'ops/ci/run-release-source-contracts.sh' \
  'trivy fs --scanners secret,misconfig' \
  'cargo deny --locked check advisories bans licenses sources' \
  'cargo audit --deny warnings' \
  'ops/benchmark-memory.sh' \
  '--profile acceptance' \
  '--gateway-limit-mib 256'; do
  grep -Fq -- "$required" "$workflow" || fail "missing quality gate: $required"
done
grep -Fq 'wiremock = ' "$repository/Cargo.toml" || fail "mock-upstream test dependency is absent"

grep -Fq 'ops/ci/validate-release-inputs.sh' "$repository/.github/workflows/ci.yml"
grep -Fq 'ops/ci/run-release-source-contracts.sh' "$repository/.github/workflows/ci.yml"
grep -Fq 'ops/ci/validate-release-inputs.sh' "$publisher"
grep -Fq 'exact)' "$validator"
grep -Fq '${registry}/memeloop-token-center:${tag}' "$validator"
grep -Fq '${registry}/memeloop-token-center-importer:${tag}' "$validator"
grep -Fq '${registry}/memeloop-token-center-plugin-installer:${tag}' "$validator"
if grep -Eqi ':(latest|master)([^a-z0-9_-]|$)|tag=(latest|master)' "$publisher" "$validator"; then
  fail "moving latest/master tags are forbidden"
fi

for required in \
  'unset HARBOR_USERNAME HARBOR_PASSWORD COSIGN_PRIVATE_KEY COSIGN_PASSWORD COSIGN_PUBLIC_KEY' \
  'buildctl --addr "$BUILDKIT_HOST" build' \
  '--opt attest:sbom=' \
  '--opt attest:provenance=mode=max' \
  'oci-artifact=true' \
  '"containerimage.digest"' \
  'crane digest "$tagged_reference"' \
  'trivy image --image-src remote' \
  'cosign sign --yes --key env://COSIGN_PRIVATE_KEY' \
  'cosign verify --key env://COSIGN_PUBLIC_KEY' \
  'cosign attest --yes --key env://COSIGN_PRIVATE_KEY' \
  'cosign verify-attestation --key env://COSIGN_PUBLIC_KEY' \
  'release must contain exactly three images'; do
  grep -Fq -- "$required" "$publisher" || fail "missing release control: $required"
done
grep -Fq '[ ! -S /var/run/docker.sock ]' "$publisher"
grep -Fq '[ ! -S /run/containerd/containerd.sock ]' "$publisher"
grep -Fq '[ ! -S /var/run/crio/crio.sock ]' "$publisher"
grep -Fq 'unix:///run/user/*/buildkit/buildkitd.sock' "$publisher"
grep -Fq '[ -S "$buildkit_socket" ]' "$publisher"
if grep -Eq 'BUILDKIT_HOST.*tcp://|unix://\* \| tcp://' "$publisher"; then
  fail "publisher must reject TCP and arbitrary Unix BuildKit endpoints"
fi
if grep -Eq '(^|[[:space:]])docker[[:space:]]+(build|push|run|login)' "$publisher"; then
  fail "publisher must use isolated rootless BuildKit, never Docker"
fi

for required in \
  'crane manifest "$image@$index_digest"' \
  'crane blob "$image@$layer_digest"' \
  'vnd.docker.reference.digest' \
  'application/vnd.in-toto+json' \
  'application/vnd.docker.attestation.manifest.v1+json' \
  'https://in-toto.io/Statement/v1' \
  '.predicateType == $predicate' \
  '(.digest.sha256 == $subject)' \
  'verified SPDX SBOM statement is missing' \
  'verified SLSA provenance statement is missing'; do
  grep -Fq -- "$required" "$attestation_verifier" || \
    fail "missing deep attestation validation: $required"
done
grep -Fq 'tests/ops/forgejo-attestation-fixtures.sh' "$shared_contracts"

grep -Fq 'https://git.k3s.onetwo.website' "$runbook"
grep -Fq 'http://forgejo-http.forgejo.svc.cluster.local:3000' "$runbook"
grep -Fq 'mtc-ci/memeloop-token-center' "$runbook"
grep -Fq '2026-09-30' "$runbook"
grep -Fq 'GitHub Actions and GHCR' "$runbook"

echo 'Forgejo Harbor release contract OK'
