#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repository"

# GitHub is the sole release entry point. Keep release policy checks together.
tests/ops/release-packaging-contract.sh
tests/ops/github-workflow-policy-fixtures.sh
