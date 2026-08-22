#!/bin/sh
set -eu

fail() {
  echo "BuildKit attestation verification: $*" >&2
  exit 1
}

validate_digest() {
  candidate=$1
  label=$2
  case "$candidate" in
    sha256:*) ;;
    *) fail "$label is not a sha256 digest" ;;
  esac
  hex=${candidate#sha256:}
  case "$hex" in
    *[!0-9a-f]* | '') fail "$label contains non-lowercase-hex characters" ;;
  esac
  [ "${#hex}" -eq 64 ] || fail "$label does not contain exactly 64 hexadecimal characters"
}

image=${1:-}
index_digest=${2:-}
index_file=${3:-}
evidence_dir=${4:-}
case "$image" in
  '' | *@* | *[!a-z0-9./:_-]*) fail "image repository is invalid" ;;
esac
validate_digest "$index_digest" "published index digest"
[ -n "$index_file" ] || fail "index evidence path is required"
[ -d "$evidence_dir" ] || fail "evidence directory does not exist"
for tool in crane jq; do
  command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done

crane manifest "$image@$index_digest" >"$index_file"
jq -e '
  .schemaVersion == 2 and
  .mediaType == "application/vnd.oci.image.index.v1+json" and
  (.manifests | type == "array" and length >= 2) and
  ([.manifests[] |
    select((.annotations["vnd.docker.reference.type"] // "") != "attestation-manifest")] |
    length == 1) and
  ([.manifests[] |
    select((.annotations["vnd.docker.reference.type"] // "") != "attestation-manifest")][0] |
    .mediaType == "application/vnd.oci.image.manifest.v1+json" and
    .platform.os == "linux" and
    .platform.architecture == "amd64") and
  all(.manifests[];
    if ((.annotations["vnd.docker.reference.type"] // "") == "attestation-manifest") then
      .mediaType == "application/vnd.oci.image.manifest.v1+json" and
      .platform.os == "unknown" and
      .platform.architecture == "unknown"
    else
      true
    end)
' "$index_file" >/dev/null || fail "published digest is not a valid attested OCI index"

subject_digests=$(jq -r '
  .manifests[] |
  select((.annotations["vnd.docker.reference.type"] // "") != "attestation-manifest") |
  select(.platform.os == "linux" and .platform.architecture == "amd64") |
  .digest
' "$index_file")
[ "$(printf '%s\n' "$subject_digests" | sed '/^$/d' | wc -l | tr -d ' ')" -eq 1 ] || \
  fail "OCI index must contain exactly one linux/amd64 subject manifest"
subject_digest=$subject_digests
validate_digest "$subject_digest" "linux/amd64 subject digest"
subject_hex=${subject_digest#sha256:}

attestation_rows=$(jq -r '
  .manifests[] |
  select(
    .mediaType == "application/vnd.oci.image.manifest.v1+json" and
    .platform.os == "unknown" and
    .platform.architecture == "unknown" and
    .annotations["vnd.docker.reference.type"] == "attestation-manifest"
  ) |
  [.digest, (.annotations["vnd.docker.reference.digest"] // "")] | @tsv
' "$index_file")
[ -n "$attestation_rows" ] || fail "OCI index contains no attestation manifests"
sbom_marker="$evidence_dir/.sbom-verified"
provenance_marker="$evidence_dir/.provenance-verified"
rm -f -- "$sbom_marker" "$provenance_marker"

tab=$(printf '\t')
while IFS="$tab" read -r attestation_digest referenced_digest; do
  [ -n "$attestation_digest" ] || continue
  validate_digest "$attestation_digest" "attestation manifest digest"
  validate_digest "$referenced_digest" "attestation subject annotation"
  [ "$referenced_digest" = "$subject_digest" ] || \
    fail "attestation descriptor references a different image manifest"

  attestation_hex=${attestation_digest#sha256:}
  attestation_file="$evidence_dir/attestation-manifest-$attestation_hex.json"
  crane manifest "$image@$attestation_digest" >"$attestation_file"
  jq -e --arg subject "$subject_digest" '
    .schemaVersion == 2 and
    .mediaType == "application/vnd.oci.image.manifest.v1+json" and
    .artifactType == "application/vnd.docker.attestation.manifest.v1+json" and
    (.layers | type == "array" and length > 0) and
    (.subject | type == "object") and
    .subject.mediaType == "application/vnd.oci.image.manifest.v1+json" and
    .subject.digest == $subject
  ' "$attestation_file" >/dev/null || \
    fail "attestation is not a native OCI artifact for the selected subject"

  layer_rows=$(jq -r '
    .layers[] |
    select(.mediaType == "application/vnd.in-toto+json") |
    [.digest, (.annotations["in-toto.io/predicate-type"] // "")] | @tsv
  ' "$attestation_file")
  [ -n "$layer_rows" ] || fail "attestation manifest has no in-toto layer"
  while IFS="$tab" read -r layer_digest predicate_type; do
    [ -n "$layer_digest" ] || continue
    validate_digest "$layer_digest" "in-toto layer digest"
    case "$predicate_type" in
      https://spdx.dev/Document) marker=$sbom_marker ;;
      https://slsa.dev/provenance/v1 | https://slsa.dev/provenance/v0.2)
        marker=$provenance_marker ;;
      *) fail "unrecognized or missing in-toto predicate type" ;;
    esac
    layer_hex=${layer_digest#sha256:}
    statement="$evidence_dir/in-toto-$layer_hex.json"
    crane blob "$image@$layer_digest" >"$statement"
    jq -e --arg predicate "$predicate_type" --arg subject "$subject_hex" --arg image "$image" '
      ((.["_type"] == "https://in-toto.io/Statement/v0.1") or
       (.["_type"] == "https://in-toto.io/Statement/v1")) and
      .predicateType == $predicate and
      ((.predicate | type) == "object") and
      (if $predicate == "https://spdx.dev/Document" then
         .predicate.SPDXID == "SPDXRef-DOCUMENT" and
         ((.predicate.spdxVersion | type) == "string") and
         (.predicate.spdxVersion | startswith("SPDX-"))
       elif $predicate == "https://slsa.dev/provenance/v1" then
         ((.predicate.buildDefinition | type) == "object") and
         ((.predicate.runDetails | type) == "object")
       elif $predicate == "https://slsa.dev/provenance/v0.2" then
         ((.predicate.buildType | type) == "string") and
         ((.predicate.buildType | length) > 0) and
         ((.predicate.builder | type) == "object")
       else
         false
       end) and
      (.subject | type == "array" and length > 0) and
      all(.subject[];
        (.name | type == "string") and
        ((.name == $image) or
         ((.name | startswith("pkg:docker/" + $image + "@")) and
          (.name | endswith("?platform=linux%2Famd64")))) and
        (.digest | type == "object") and
        ((.digest | keys) == ["sha256"]) and
        (.digest.sha256 == $subject))
    ' "$statement" >/dev/null || \
      fail "in-toto statement type, predicate structure, image name, or exact sha256 subject is invalid"
    : >"$marker"
  done <<EOF
$layer_rows
EOF
done <<EOF
$attestation_rows
EOF

[ -f "$sbom_marker" ] || fail "verified SPDX SBOM statement is missing"
[ -f "$provenance_marker" ] || fail "verified SLSA provenance statement is missing"
rm -f -- "$sbom_marker" "$provenance_marker"
echo "BuildKit SBOM and provenance statements verified for $image@$index_digest"
