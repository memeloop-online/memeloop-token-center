#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repository"

# GitHub and the temporary Forgejo release workflow invoke this same entry
# point so immutable-image and workflow policy cannot silently diverge.
tests/ops/release-packaging-contract.sh
tests/ops/github-workflow-policy-fixtures.sh
tests/ops/forgejo-harbor-release-contract.sh
tests/ops/forgejo-attestation-fixtures.sh
