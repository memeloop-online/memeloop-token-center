#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec python3 "$repository/tests/load/postgres_explain.py" "$@"
