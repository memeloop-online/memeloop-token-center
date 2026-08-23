#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec node "$repository/tests/load/postgres_explain.ts" "$@"
