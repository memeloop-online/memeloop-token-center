#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
workspace=$(mktemp -d "${TMPDIR:-/tmp}/mtc-cpa-audit.XXXXXX")
cleanup() {
  rm -rf -- "$workspace"
}
trap cleanup EXIT HUP INT TERM

printf '%s\n' '#!/bin/sh' 'printf "%s\\n" "$FAKE_PSQL_COUNTS"' >"$workspace/psql"
chmod 0555 "$workspace/psql"
export PATH="$workspace:$PATH"
export PGHOST=postgres.example.test
export PGUSER=fixture
export PGPASSWORD=fixture-only-password
export PGDATABASE=fixture
export IMPORT_TENANT_EXTERNAL_ID=cpa-fixture
export CPAMP_IMPORT_SOURCE=cpamp-fixture-v1
export SESSION_ARCHIVE_IMPORT_SOURCE=archive-fixture-v2
export EXPECTED_CPAMP_EVENTS=4
export EXPECTED_ARCHIVE_RECORDS=5
export FAKE_PSQL_COUNTS='4|4|5|5000|5|3|2|3|2|0|0|0|2|5|1'

"$repository/ops/audit-cpa-migration.sh" >"$workspace/pass.json"
grep -Fq '"cpamp_checkpoint":4' "$workspace/pass.json"
grep -Fq '"archive_checkpoint":5' "$workspace/pass.json"
grep -Fq '"archive_exact":3' "$workspace/pass.json"
grep -Fq '"archive_unlinked":2' "$workspace/pass.json"
grep -Fq '"conversation_clusters":2' "$workspace/pass.json"
grep -Fq '"conversation_observations":5' "$workspace/pass.json"
grep -Fq '"conversation_edges":1' "$workspace/pass.json"
grep -Fq '"gap_locators":0' "$workspace/pass.json"

EXPECTED_ARCHIVE_RECORDS=6
export EXPECTED_ARCHIVE_RECORDS
if "$repository/ops/audit-cpa-migration.sh" >"$workspace/count.out" 2>"$workspace/count.err"; then
  echo "archive source/target count mismatch unexpectedly passed" >&2
  exit 1
fi
grep -Fq 'archive source, checkpoint, correlation, projection, or quarantine counts disagree'   "$workspace/count.err"

EXPECTED_ARCHIVE_RECORDS=5
FAKE_PSQL_COUNTS='4|4|5|5000|5|3|2|3|2|1|0|0|2|5|1'
export EXPECTED_ARCHIVE_RECORDS FAKE_PSQL_COUNTS
if "$repository/ops/audit-cpa-migration.sh" >"$workspace/quarantine.out" 2>"$workspace/quarantine.err"; then
  echo "unresolved archive quarantine unexpectedly passed" >&2
  exit 1
fi
grep -Fq 'archive source, checkpoint, correlation, projection, or quarantine counts disagree'   "$workspace/quarantine.err"

FAKE_PSQL_COUNTS='4|4|5|5000|5|3|2|3|2|0|1|0|2|5|1'
export FAKE_PSQL_COUNTS
if "$repository/ops/audit-cpa-migration.sh" >"$workspace/gap.out" 2>"$workspace/gap.err"; then
  echo "remaining archive gap locator unexpectedly passed" >&2
  exit 1
fi
grep -Fq 'archive source, checkpoint, correlation, projection, or quarantine counts disagree'   "$workspace/gap.err"

echo "CPA migration audit contract: PASS"
