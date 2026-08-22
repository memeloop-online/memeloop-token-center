#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
verifier="$repository/ops/ci/verify-buildkit-attestations.sh"
fixture=$(mktemp -d "${TMPDIR:-/tmp}/mtc-attestation-fixture.XXXXXX")
cleanup() { rm -rf -- "$fixture"; }
trap cleanup EXIT HUP INT TERM

index=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
subject=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
attestation_manifest=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
sbom_layer=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
provenance_layer=ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
wrong=9999999999999999999999999999999999999999999999999999999999999999
image=registry.example.test/mtc/service
subject_name="pkg:docker/$image@sha-test?platform=linux%2Famd64"

mkdir -p "$fixture/bin" "$fixture/evidence"
cat >"$fixture/bin/crane" <<'SH'
#!/bin/sh
set -eu
command=$1
reference=$2
digest=${reference##*@}
hex=${digest#sha256:}
case "$command" in
  manifest) cat "$FIXTURE_ROOT/manifest-$hex.json" ;;
  blob) cat "$FIXTURE_ROOT/blob-$hex.json" ;;
  *) exit 64 ;;
esac
SH
chmod 0755 "$fixture/bin/crane"

write_good() {
  cat >"$fixture/manifest-$index.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[
 {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:$subject","platform":{"os":"linux","architecture":"amd64"}},
 {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:$attestation_manifest","platform":{"os":"unknown","architecture":"unknown"},"annotations":{"vnd.docker.reference.type":"attestation-manifest","vnd.docker.reference.digest":"sha256:$subject"}}
]}
EOF
  cat >"$fixture/manifest-$attestation_manifest.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","artifactType":"application/vnd.docker.attestation.manifest.v1+json","subject":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:$subject"},"layers":[
 {"mediaType":"application/vnd.in-toto+json","digest":"sha256:$sbom_layer","annotations":{"in-toto.io/predicate-type":"https://spdx.dev/Document"}},
 {"mediaType":"application/vnd.in-toto+json","digest":"sha256:$provenance_layer","annotations":{"in-toto.io/predicate-type":"https://slsa.dev/provenance/v1"}}
]}
EOF
  cat >"$fixture/blob-$sbom_layer.json" <<EOF
{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"$subject_name","digest":{"sha256":"$subject"}}],"predicateType":"https://spdx.dev/Document","predicate":{"SPDXID":"SPDXRef-DOCUMENT"}}
EOF
  cat >"$fixture/blob-$provenance_layer.json" <<EOF
{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"$subject_name","digest":{"sha256":"$subject"}}],"predicateType":"https://slsa.dev/provenance/v1","predicate":{"buildDefinition":{}}}
EOF
}

verify() {
  PATH="$fixture/bin:$PATH" FIXTURE_ROOT="$fixture" \
    "$verifier" "$image" "sha256:$index" \
    "$fixture/index-output.json" "$fixture/evidence"
}

expect_rejected() {
  label=$1
  if verify >"$fixture/$label.out" 2>"$fixture/$label.err"; then
    echo "malicious attestation fixture was accepted: $label" >&2
    exit 1
  fi
}

write_good
verify >/dev/null

# A valid predicate attached to a different subject must never be credited to
# the image that was just built.
cat >"$fixture/blob-$sbom_layer.json" <<EOF
{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"$subject_name","digest":{"sha256":"$wrong"}}],"predicateType":"https://spdx.dev/Document","predicate":{}}
EOF
expect_rejected wrong-subject

# A digest match cannot bind evidence to a different repository. The package
# URL name must identify the dynamic image and the selected linux/amd64 result.
write_good
cat >"$fixture/blob-$sbom_layer.json" <<EOF
{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"pkg:docker/registry.example.test/other/service@sha-test?platform=linux%2Famd64","digest":{"sha256":"$subject"}}],"predicateType":"https://spdx.dev/Document","predicate":{}}
EOF
expect_rejected wrong-subject-name

# Descriptor annotations cannot substitute for the predicate type inside the
# in-toto statement.
write_good
cat >"$fixture/blob-$sbom_layer.json" <<EOF
{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"$subject_name","digest":{"sha256":"$subject"}}],"predicateType":"https://example.test/not-spdx","predicate":{}}
EOF
expect_rejected predicate-mismatch

# Short, uppercase, or otherwise malformed registry digests fail before blob
# retrieval. This fixture uses a short layer digest.
write_good
cat >"$fixture/manifest-$attestation_manifest.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","artifactType":"application/vnd.docker.attestation.manifest.v1+json","subject":{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:$subject"},"layers":[
 {"mediaType":"application/vnd.in-toto+json","digest":"sha256:abc","annotations":{"in-toto.io/predicate-type":"https://spdx.dev/Document"}},
 {"mediaType":"application/vnd.in-toto+json","digest":"sha256:$provenance_layer","annotations":{"in-toto.io/predicate-type":"https://slsa.dev/provenance/v1"}}
]}
EOF
expect_rejected malformed-layer-digest

# An attestation descriptor that points at another platform manifest cannot
# claim the selected linux/amd64 subject.
write_good
python3 - "$fixture/manifest-$index.json" "$wrong" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
payload = json.loads(path.read_text())
payload["manifests"][1]["annotations"]["vnd.docker.reference.digest"] = "sha256:" + sys.argv[2]
path.write_text(json.dumps(payload))
PY
expect_rejected descriptor-subject-mismatch

# A compatibility image manifest without an OCI artifactType/subject is not
# native OCI attestation evidence and must fail closed.
write_good
cat >"$fixture/manifest-$attestation_manifest.json" <<EOF
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[
 {"mediaType":"application/vnd.in-toto+json","digest":"sha256:$sbom_layer","annotations":{"in-toto.io/predicate-type":"https://spdx.dev/Document"}},
 {"mediaType":"application/vnd.in-toto+json","digest":"sha256:$provenance_layer","annotations":{"in-toto.io/predicate-type":"https://slsa.dev/provenance/v1"}}
]}
EOF
expect_rejected non-native-manifest

# The OCI artifact subject field is independently authoritative; matching only
# the parent index annotation is insufficient.
write_good
python3 - "$fixture/manifest-$attestation_manifest.json" "$wrong" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
payload = json.loads(path.read_text())
payload["subject"]["digest"] = "sha256:" + sys.argv[2]
path.write_text(json.dumps(payload))
PY
expect_rejected manifest-subject-mismatch

# An attested single-platform release must not hide another runnable image.
write_good
python3 - "$fixture/manifest-$index.json" "$wrong" <<'PY'
import json, pathlib, sys
path = pathlib.Path(sys.argv[1])
payload = json.loads(path.read_text())
payload["manifests"].append({
    "mediaType": "application/vnd.oci.image.manifest.v1+json",
    "digest": "sha256:" + sys.argv[2],
    "platform": {"os": "linux", "architecture": "arm64"},
})
path.write_text(json.dumps(payload))
PY
expect_rejected extra-runnable-manifest

echo 'Forgejo attestation malicious fixtures OK'
