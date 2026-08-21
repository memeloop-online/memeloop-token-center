#!/bin/sh
set -eu

fail() {
  echo "Harbor release: $*" >&2
  exit 1
}

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
: "${HARBOR_REPOSITORY_PREFIX:?HARBOR_REPOSITORY_PREFIX is required}"
: "${HARBOR_USERNAME:?HARBOR_USERNAME is required}"
: "${HARBOR_PASSWORD:?HARBOR_PASSWORD is required}"
: "${COSIGN_PRIVATE_KEY:?COSIGN_PRIVATE_KEY is required}"
: "${COSIGN_PASSWORD:?COSIGN_PASSWORD is required}"
: "${COSIGN_PUBLIC_KEY:?COSIGN_PUBLIC_KEY is required}"
: "${RELEASE_REVISION:?RELEASE_REVISION is required}"
: "${RELEASE_EVIDENCE_DIR:?RELEASE_EVIDENCE_DIR is required}"
: "${BUILDKIT_HOST:?BUILDKIT_HOST must point at the isolated rootless builder}"

for tool in buildctl cosign crane jq trivy; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool is not installed on the release runner"
done
[ ! -S /var/run/docker.sock ] || fail "the host Docker socket must not be mounted"

case "$BUILDKIT_HOST" in
  unix://* | tcp://*) ;;
  *) fail "BUILDKIT_HOST must use an explicit unix:// or tcp:// endpoint" ;;
esac

cd "$repository"
release_rows=$(ops/ci/validate-release-inputs.sh \
  "$HARBOR_REPOSITORY_PREFIX" "$RELEASE_REVISION" exact)
mkdir -p "$RELEASE_EVIDENCE_DIR"
chmod 0700 "$RELEASE_EVIDENCE_DIR"
docker_config=$(mktemp -d "${RUNNER_TEMP:-/tmp}/mtc-harbor-auth.XXXXXX")
cleanup() {
  rm -rf -- "$docker_config"
}
trap cleanup EXIT HUP INT TERM
chmod 0700 "$docker_config"
export DOCKER_CONFIG=$docker_config

auth=$(printf '%s' "$HARBOR_USERNAME:$HARBOR_PASSWORD" | base64 | tr -d '\n')
host=${HARBOR_REPOSITORY_PREFIX%%/*}
jq -n --arg host "$host" --arg auth "$auth" \
  '{auths: {($host): {auth: $auth}}}' >"$DOCKER_CONFIG/config.json"
chmod 0600 "$DOCKER_CONFIG/config.json"
unset auth

commit_timestamp=$(git show -s --format=%cI "$RELEASE_REVISION")
source_url=${FORGEJO_SERVER_URL:-https://forgejo.invalid}/${FORGEJO_REPOSITORY:-unknown/unknown}
release_manifest="$RELEASE_EVIDENCE_DIR/harbor-release-$RELEASE_REVISION.jsonl"
: >"$release_manifest"
chmod 0600 "$release_manifest"

verify_buildkit_attestations() {
  image=$1
  digest=$2
  index_file=$3
  crane manifest "$image@$digest" >"$index_file"
  jq -e '.manifests | type == "array"' "$index_file" >/dev/null || \
    fail "$image did not publish an OCI index containing attestations"
  sbom=false
  provenance=false
  for attestation_digest in $(jq -r '
    .manifests[] |
    select(.annotations["vnd.docker.reference.type"] == "attestation-manifest") |
    .digest' "$index_file"); do
    attestation_file="$RELEASE_EVIDENCE_DIR/attestation-${attestation_digest#sha256:}.json"
    crane manifest "$image@$attestation_digest" >"$attestation_file"
    predicates=$(jq -r '.layers[]?.annotations["in-toto.io/predicate-type"] // empty' \
      "$attestation_file")
    if printf '%s\n' "$predicates" | grep -Eiq 'spdx'; then
      sbom=true
    fi
    if printf '%s\n' "$predicates" | grep -Eq 'https://slsa.dev/provenance/'; then
      provenance=true
    fi
  done
  [ "$sbom" = true ] || fail "$image is missing its BuildKit SPDX SBOM attestation"
  [ "$provenance" = true ] || fail "$image is missing its BuildKit SLSA provenance attestation"
}

printf '%s\n' "$release_rows" | while IFS='|' read -r scope dockerfile image_name tagged_reference; do
  [ -n "$scope" ] || continue
  image=${tagged_reference%:*}
  if crane digest "$tagged_reference" >/dev/null 2>&1; then
    fail "$tagged_reference already exists; immutable SHA tags are never overwritten"
  fi

  metadata="$RELEASE_EVIDENCE_DIR/$scope-build-metadata.json"
  set -- buildctl --addr "$BUILDKIT_HOST" build \
    --frontend dockerfile.v0 \
    --local context=. \
    --local dockerfile=. \
    --opt "filename=$dockerfile" \
    --opt platform=linux/amd64 \
    --opt "label:org.opencontainers.image.source=$source_url" \
    --opt "label:org.opencontainers.image.revision=$RELEASE_REVISION" \
    --opt attest:sbom= \
    --opt attest:provenance=mode=max \
    --output "type=image,name=$tagged_reference,push=true,oci-mediatypes=true,name-canonical=true" \
    --metadata-file "$metadata" \
    --progress plain
  if [ "$scope" = service ]; then
    set -- "$@" \
      --opt "build-arg:MTC_BUILD_GIT_SHA_INPUT=$RELEASE_REVISION" \
      --opt "build-arg:MTC_BUILD_TIMESTAMP_INPUT=$commit_timestamp" \
      --opt build-arg:MTC_BUILD_TARGET_INPUT=linux/amd64
  fi
  if [ "$scope" = plugin-installer ]; then
    set -- "$@" --opt build-arg:TARGETARCH=amd64
  fi
  "$@"

  digest=$(jq -r '."containerimage.digest" // empty' "$metadata")
  digest_hex=${digest#sha256:}
  case "$digest_hex" in
    *[!0-9a-f]* | '') fail "$scope build returned an invalid digest" ;;
  esac
  [ "${#digest_hex}" -eq 64 ] || fail "$scope digest does not contain 64 hexadecimal characters"
  resolved=$(crane digest "$tagged_reference")
  [ "$resolved" = "$digest" ] || \
    fail "$tagged_reference resolved to $resolved instead of $digest"
  digest_reference="$image@$digest"

  config="$RELEASE_EVIDENCE_DIR/$scope-image-config.json"
  crane config --platform linux/amd64 "$digest_reference" >"$config"
  jq -e --arg revision "$RELEASE_REVISION" \
    '.config.Labels["org.opencontainers.image.revision"] == $revision' "$config" >/dev/null || \
    fail "$scope image revision label does not match the release commit"
  [ "$(jq -r '.config.User // empty' "$config")" = '10001:10001' ] || \
    fail "$scope image does not run as 10001:10001"

  case "$scope" in
    service) expected_entrypoint=/usr/local/bin/memeloop-token-center ;;
    importer) expected_entrypoint=/usr/local/bin/migrate-cpamp ;;
    plugin-installer) expected_entrypoint=/usr/local/bin/install-plugin-oci ;;
    *) fail "unexpected image scope $scope" ;;
  esac
  jq -e --arg expected "$expected_entrypoint" \
    '.config.Entrypoint == [$expected]' "$config" >/dev/null || \
    fail "$scope image entrypoint is not the hardened executable"

  verify_buildkit_attestations "$image" "$digest" \
    "$RELEASE_EVIDENCE_DIR/$scope-index.json"
  trivy image --image-src remote --scanners vuln --severity HIGH,CRITICAL \
    --exit-code 1 "$digest_reference"

  predicate="$RELEASE_EVIDENCE_DIR/$scope-release-predicate.json"
  jq -n \
    --arg revision "$RELEASE_REVISION" \
    --arg image "$image" \
    --arg digest "$digest" \
    --arg source "$source_url" \
    --arg run_id "${FORGEJO_RUN_ID:-unknown}" \
    '{schema_version: 1, revision: $revision, image: $image, digest: $digest,
      source: $source, forgejo_run_id: $run_id, quality_gates: "passed"}' \
    >"$predicate"
  cosign sign --yes --key env://COSIGN_PRIVATE_KEY --tlog-upload=false "$digest_reference"
  cosign verify --key env://COSIGN_PUBLIC_KEY --insecure-ignore-tlog "$digest_reference" \
    >"$RELEASE_EVIDENCE_DIR/$scope-signature-verification.json"
  cosign attest --yes --key env://COSIGN_PRIVATE_KEY --tlog-upload=false \
    --type custom --predicate "$predicate" "$digest_reference"
  cosign verify-attestation --key env://COSIGN_PUBLIC_KEY --insecure-ignore-tlog \
    --type custom "$digest_reference" \
    >"$RELEASE_EVIDENCE_DIR/$scope-attestation-verification.json"

  jq -nc \
    --arg scope "$scope" \
    --arg image "$image" \
    --arg tag "$RELEASE_REVISION" \
    --arg digest "$digest" \
    --arg revision "$RELEASE_REVISION" \
    '{schema_version: 1, scope: $scope, image: $image, tag: $tag,
      digest: $digest, revision: $revision, reference: ($image + "@" + $digest),
      sbom: "verified", provenance: "verified", signature: "verified",
      release_attestation: "verified", vulnerability_gate: "passed"}' \
    >>"$release_manifest"
done

final_manifest="$RELEASE_EVIDENCE_DIR/harbor-release-$RELEASE_REVISION.json"
jq -s --arg revision "$RELEASE_REVISION" '
  if length != 3 then error("release must contain exactly three images") else . end |
  if (map(.revision == $revision) | all) then . else error("revision mismatch") end |
  if (map(.scope) | sort) == ["importer", "plugin-installer", "service"] then .
  else error("release image set is incomplete") end
' "$release_manifest" >"$final_manifest"
chmod 0644 "$final_manifest"
echo "Harbor release verified by immutable digest: $final_manifest"
