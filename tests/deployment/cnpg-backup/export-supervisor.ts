import assert from 'node:assert/strict';

export function superviseExport(script: string): string {
  const replace = (before: string, after: string) => {
    assert.equal(script.split(before).length, 2);
    script = script.replace(before, after);
  };
  replace('space_guard_pid=\n', '');
  const cleanupStart = script.indexOf('  if [ -n "$space_guard_pid" ]; then');
  const cleanupEnd = script.indexOf('  head -c 4096 /tmp/stage.stdout', cleanupStart);
  assert.ok(cleanupStart > 0 && cleanupEnd > cleanupStart);
  script = script.slice(0, cleanupStart) + String.raw`  for owned_pid in "$dump_pid" "$rate_pid"; do
    if test -n "$owned_pid"; then
      kill -TERM -- "-$owned_pid" 2>/dev/null || true
      kill -TERM "$owned_pid" 2>/dev/null || true
    fi
  done
  if test -n "$dump_pid$rate_pid"; then sleep 2; fi
  for owned_pid in "$dump_pid" "$rate_pid"; do
    if test -n "$owned_pid"; then
      kill -KILL -- "-$owned_pid" 2>/dev/null || true
      kill -KILL "$owned_pid" 2>/dev/null || true
      wait "$owned_pid" 2>/dev/null || true
    fi
  done
` + script.slice(cleanupEnd);
  const guardStart = script.indexOf('stage_pid=$$\n');
  const guardEnd = script.indexOf('space_guard_pid=$!\n', guardStart);
  assert.ok(guardStart > 0 && guardEnd > guardStart);
  script = script.slice(0, guardStart) + String.raw`stage_guard() {
  if ! source_space_lease; then
    printf 'guard_abort=source-lease utc=%s\n' "$(date -u +%FT%TZ)" >&2
    return 1
  fi
  if ! capacity_backup; then
    printf 'guard_abort=volume-identity-or-capacity utc=%s\n' "$(date -u +%FT%TZ)" >&2
    return 1
  fi
}
` + script.slice(guardEnd + 'space_guard_pid=$!\n'.length);
  replace('prlimit --fsize=25769803776:25769803776 -- /bin/sh', 'setsid prlimit --fsize=25769803776:25769803776 -- /bin/sh');
  replace('pg_dump --dbname=', 'setsid pg_dump --dbname=');
  replace('dump_pid=$!\n', 'dump_pid=$!\nprintf \'export_supervisor=%s producer=%s writer=%s utc=%s\\n\' "$$" "$dump_pid" "$rate_pid" "$(date -u +%FT%TZ)"\n');
  replace('wait "$dump_pid"\ndump_pid=\nwait "$rate_pid"\nrate_pid=', String.raw`while kill -0 "$dump_pid" 2>/dev/null; do
  stage_guard
  sleep 2
done
wait "$dump_pid"
while kill -0 "$rate_pid" 2>/dev/null; do
  stage_guard
  sleep 2
done
wait "$rate_pid"
stage_guard
dump_pid=
rate_pid=`);
  return script;
}
