#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
target=${1:?usage: prepare-cpamp-acceptance-bundle.sh <absolute-empty-directory>}
case "$target" in
  /*) ;;
  *) echo "acceptance bundle target must be absolute" >&2; exit 2 ;;
esac
target_parent=${target%/*}
target_name=${target##*/}
case "$target_name" in ''|.|..) echo "invalid acceptance bundle directory name" >&2; exit 2;; esac
physical_parent=$(CDPATH='' cd -P -- "$target_parent" 2>/dev/null && pwd) || {
  echo "acceptance bundle parent must be an existing directory" >&2
  exit 2
}
[ "$target_parent" = "$physical_parent" ] || {
  echo "acceptance bundle parent must be a canonical path without symlinks" >&2
  exit 2
}
[ ! -L "$target" ] || { echo "acceptance bundle target must not be a symlink" >&2; exit 2; }
if [ -e "$target" ]; then
  [ -d "$target" ] || { echo "acceptance bundle target is not a directory" >&2; exit 2; }
  [ -z "$(find "$target" -mindepth 1 -maxdepth 1 -print -quit)" ] || {
    echo "acceptance bundle target must be empty" >&2
    exit 2
  }
else
  mkdir -m 0700 -- "$target"
fi
chmod 0700 -- "$target"

copy_regular() {
  source_file=$1
  destination_name=$2
  [ -f "$source_file" ] && [ ! -L "$source_file" ] || {
    echo "required CPAMP acceptance asset is not a regular non-symlink file: $source_file" >&2
    exit 2
  }
  cp -- "$source_file" "$target/$destination_name"
}

copy_regular "$repository/tests/ops/cpamp-import-postgres-acceptance.sh"   cpamp-import-postgres-acceptance.sh
copy_regular "$repository/ops/migrate-cpamp.sh" migrate-cpamp.sh
copy_regular "$repository/tests/fixtures/cpamp/initial.sql" initial.sql
for migration in 0001_initial 0002_query_indexes 0004_request_events   0005_generation_jobs 0018_model_price_tiers 0019_session_archive_import   0021_request_locators 0022_budget_rollups 0023_generation_daily_aggregates   0024_request_stats_rollups 0027_cpamp_source_digests; do
  postgres_source="$repository/migrations/postgres/$migration.sql"
  common_source="$repository/migrations/common/$migration.sql"
  if [ -e "$postgres_source" ] || [ -L "$postgres_source" ]; then
    copy_regular "$postgres_source" "$migration.sql"
  elif [ -e "$common_source" ] || [ -L "$common_source" ]; then
    copy_regular "$common_source" "$migration.sql"
  else
    echo "required CPAMP acceptance migration is missing: $migration" >&2
    exit 2
  fi
done
chmod 0555 -- "$target/cpamp-import-postgres-acceptance.sh" "$target/migrate-cpamp.sh"
chmod 0444 -- "$target"/*.sql
chmod 0555 -- "$target"
chmod u-s,g-s,o-t -- "$target"
