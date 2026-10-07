import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import test from 'node:test';
import { assertSourceFresh, parseSourceStat, sourceFilesystem, sourceStatCommand, verifySourceBinding } from './source-filesystem.ts';
import { sourceInventory, sourceStatOutput } from './source-filesystem.fixture.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');

test('source identity and numeric stat counters fail closed without relaxing reviewed limits', () => {
  const value = sourceInventory();
  verifySourceBinding(value.pod, value.claim, value.persistent, value.cluster);
  assert.equal(parseSourceStat(sourceStatOutput()).availableBytes, 10 * 1024 ** 3);
  for (const mutate of [
    (candidate: any) => { candidate.pod.metadata.uid = 'replaced'; },
    (candidate: any) => { candidate.pod.spec.nodeName = 'other'; },
    (candidate: any) => { candidate.pod.spec.containers[0].volumeMounts[0].subPath = 'pgdata'; },
    (candidate: any) => { candidate.pod.spec.containers[0].volumeMounts.push({ name: 'shadow', mountPath: sourceFilesystem.pgdata }); },
    (candidate: any) => { candidate.pod.spec.containers[0].volumeMounts.push({ name: 'wal', mountPath: `${sourceFilesystem.pgdata}/pg_wal` }); },
    (candidate: any) => { candidate.pod.spec.containers[0].volumeMounts.push({ name: 'base', mountPath: `${sourceFilesystem.pgdata}/base` }); },
    (candidate: any) => { candidate.claim.metadata.uid = 'other'; },
    (candidate: any) => { candidate.persistent.spec.claimRef.uid = 'other'; },
    (candidate: any) => { candidate.persistent.spec.csi.volumeHandle = 'other'; },
    (candidate: any) => { candidate.cluster.status.currentPrimary = 'other'; },
    (candidate: any) => { candidate.cluster.spec.instances = 2; },
  ]) {
    const candidate = sourceInventory();
    mutate(candidate);
    assert.throws(() => verifySourceBinding(candidate.pod, candidate.claim, candidate.persistent, candidate.cluster));
  }
  for (const output of [
    sourceStatOutput().replace('statfs=4096', 'statfs=-4096'),
    sourceStatOutput().replace('statfs=4096', 'statfs=1.5'),
    sourceStatOutput().replace('7847936', '9007199254740992'),
    sourceStatOutput().replace('7847936', '99999999'),
    sourceStatOutput().replace('15721786', '65535'),
    sourceStatOutput().replace('15721786', '15728641'),
    sourceStatOutput(99999999),
    sourceStatOutput().replaceAll(' xfs ', ' tmpfs '),
    sourceStatOutput().replaceAll(` /dev/longhorn/${sourceFilesystem.persistent} `, ' /dev/longhorn/other '),
    sourceStatOutput().replaceAll(' xfs / ', ' xfs /subpath '),
    sourceStatOutput().replace('mount_after=', 'unexpected='),
    sourceStatOutput().replace(/65:48\n$/, '65:49\n'),
    sourceStatOutput() + 'extra\n',
  ]) assert.throws(() => parseSourceStat(output));
});

test('stat freshness uses both clocks, retains 90s and classifies invalid/future/clock movement', () => {
  const now = Date.parse('2026-10-06T10:46:00Z');
  for (const [wallAge, monotonicAge, category] of [
    [90000, 90000, 'SOURCE_SAMPLE_FRESH'], [90001, 90001, 'SOURCE_SAMPLE_STALE'],
    [-5000, 0, 'SOURCE_SAMPLE_FRESH'], [-5001, 0, 'SOURCE_SAMPLE_FUTURE'],
    [0, 6000, 'SOURCE_CLOCK_INCOMPATIBLE'], [6000, 0, 'SOURCE_CLOCK_INCOMPATIBLE'],
    [0, -1, 'SOURCE_CLOCK_INCOMPATIBLE'], [NaN, 0, 'SOURCE_SAMPLE_INVALID'],
  ] as const) {
    const events: Record<string, unknown>[] = [];
    const inspect = () => assertSourceFresh({ startedWallMs: now - wallAge, startedMonotonicMs: 0 }, fields => events.push(fields), { wall: () => now, monotonic: () => monotonicAge });
    if (category === 'SOURCE_SAMPLE_FRESH') inspect();
    else assert.throws(inspect);
    assert.equal(events[0]!.category, category);
    assert.equal(events[0]!.method, 'mounted-statfs');
  }
});

test('reconciled source expansion follows the same bound volume rather than the old literal capacity', () => {
  for (const capacityGiB of [30, 40, 48]) {
    const value = sourceInventory(capacityGiB);
    const identity = verifySourceBinding(value.pod, value.claim, value.persistent, value.cluster);
    assert.equal(identity.capacityBytes, capacityGiB * 1024 ** 3);
    const sample = parseSourceStat(sourceStatOutput(2621440, capacityGiB * 262144 - 16384), identity.capacityBytes);
    assert.equal(sample.availableBytes, 10 * 1024 ** 3);
  }
  const old = sourceInventory();
  const expanded = sourceInventory(40);
  assert.notDeepEqual(verifySourceBinding(old.pod, old.claim, old.persistent, old.cluster), verifySourceBinding(expanded.pod, expanded.claim, expanded.persistent, expanded.cluster));
  assert.throws(() => parseSourceStat(sourceStatOutput(), 40 * 1024 ** 3));
  assert.throws(() => parseSourceStat(sourceStatOutput(2621440, 41 * 262144), 40 * 1024 ** 3));
  for (const mutate of [
    (candidate: any) => { candidate.claim.status.capacity.storage = '30Gi'; },
    (candidate: any) => { candidate.claim.spec.resources.requests.storage = '30Gi'; },
    (candidate: any) => { candidate.cluster.spec.storage.size = '30Gi'; },
    (candidate: any) => { candidate.claim.status.conditions = [{ type: 'FileSystemResizePending', status: 'True' }]; },
    (candidate: any) => { candidate.persistent.spec.capacity.storage = 'NaNGi'; },
    (candidate: any) => { candidate.persistent.spec.capacity.storage = '9007199254740992Gi'; },
    (candidate: any) => { candidate.claim.metadata.uid = 'replacement-volume'; },
  ]) {
    const candidate = sourceInventory(40);
    mutate(candidate);
    assert.throws(() => verifySourceBinding(candidate.pod, candidate.claim, candidate.persistent, candidate.cluster));
  }
  const smaller = sourceInventory(29);
  assert.throws(() => verifySourceBinding(smaller.pod, smaller.claim, smaller.persistent, smaller.cluster));
});

test('same CNPG image executes the exact stat shell and kills only its timed-out metadata group', { timeout: 240_000 }, context => {
  const image = 'ghcr.io/cloudnative-pg/postgresql@sha256:b1deeed2aa998b2f381e39c5cadb9ec06127708c8bd62965743af19abf21628f';
  execFileSync('docker', ['pull', image], { timeout: 180_000, stdio: ['ignore', 'pipe', 'pipe'] });
  const docker = (args: string[], input?: string) => execFileSync('docker', args, { input, encoding: 'utf8', timeout: 30_000, stdio: ['pipe', 'pipe', 'pipe'] });
  const container = docker(['run', '-d', '--network=none', '--read-only', '--cap-drop=ALL', '--security-opt=no-new-privileges', '--user=26:26', '--tmpfs', '/tmp:rw,exec,size=16m,uid=26,gid=26', '--tmpfs', '/scratch:rw,size=128m,uid=26,gid=26', '--tmpfs', `${sourceFilesystem.mount}:rw,size=32m,uid=26,gid=26`, '--env', `PGDATA=${sourceFilesystem.pgdata}`, '--entrypoint=/bin/sleep', image, '180']).trim();
  context.after(() => docker(['rm', '-f', container]));
  const shell = (script: string) => docker(['exec', container, '/bin/sh', '-ec', script]);
  const write = (path: string, text: string) => docker(['exec', '-i', container, 'dd', `of=${path}`, 'status=none'], text);
  shell('mkdir /tmp/bin "$PGDATA"');
  const actualMount = shell('findmnt -rn -o TARGET,SOURCE,FSTYPE,FSROOT,MAJ:MIN -T "$PGDATA"').trim().split(/\s+/);
  assert.equal(actualMount.length, 5);
  assert.equal(actualMount[0], sourceFilesystem.mount);
  assert.equal(actualMount[2], 'tmpfs');
  write('/tmp/bin/findmnt', `#!/bin/sh\nprintf '%s\\n' '${sourceFilesystem.mount} /dev/longhorn/${sourceFilesystem.persistent} xfs / 65:48'\n`);
  shell('chmod 700 /tmp/bin/findmnt');
  const path = shell('printf %s "$PATH"');
  const args = ['exec', '--env', `PATH=/tmp/bin:${path}`, container, ...sourceStatCommand];
  const actual = docker(args);
  assert.match(actual, /statfs=[0-9 ]+tmpfs [a-fA-F0-9]+/);
  const counters = actual.split('\n')[1]!.slice('statfs='.length).split(' ').slice(0, 6).map(Number);
  assert.ok(counters.every(Number.isSafeInteger));
  assert.ok(counters[0]! > 0 && counters[1]! > 0 && counters[3]! <= counters[2]! && counters[2]! <= counters[1]!);
  assert.ok(counters[0]! * counters[1]! <= 32 * 1024 ** 2 && counters[5]! <= counters[4]!);
  assert.throws(() => parseSourceStat(actual), 'Real fixture tmpfs must not impersonate the reviewed source xfs');
  write('/tmp/bin/stat', '#!/bin/sh\ntrap "" TERM\nsleep 120 &\nwait\n');
  shell('chmod 700 /tmp/bin/stat');
  const started = performance.now();
  assert.throws(() => docker(args), (error: any) => error.status === 137);
  const elapsed = performance.now() - started;
  assert.ok(elapsed >= 8000 && elapsed < 10000);
  assert.equal(docker(['inspect', '-f', '{{.State.Running}}', container]).trim(), 'true');
  const liveProcesses = () => docker(['top', container, '-eo', 'pid,ppid,pgid,stat,comm']).trim().split('\n').slice(1).filter(line => !line.trim().split(/\s+/)[3]!.startsWith('Z'));
  const remaining = liveProcesses();
  assert.equal(remaining.length, 1, `Metadata process leak: ${remaining.join('; ')}`);
  assert.equal(remaining[0]!.trim().split(/\s+/).at(-1), 'sleep');
  context.diagnostic(`Same-image statfs tool smoke and timeout passed; fake mount identity, real tmpfs counters; timeout wall_ms=${elapsed}; remaining=${remaining.join('; ')}`);
  shell('mkdir /scratch/socket; initdb -D /scratch/source -U postgres --auth-local=trust --auth-host=reject >/tmp/init.log; pg_ctl -D /scratch/source -l /tmp/postgres.log -o "-c listen_addresses= -c unix_socket_directories=/scratch/socket -c shared_buffers=8MB -c max_connections=10" -w start');
  docker(['exec', '-d', container, '/bin/sh', '-ec', 'printf "%s\\n" "$$" > /tmp/unrelated.pid; exec sleep 120']);
  shell('timeout 2s /bin/sh -ec \'while test ! -s /tmp/unrelated.pid; do sleep 0.01; done\'');
  const protectedProcesses = liveProcesses();
  const protectedPids = protectedProcesses.map(line => line.trim().split(/\s+/)[0]!).sort();
  assert.ok(protectedProcesses.some(line => line.trim().split(/\s+/).at(-1) === 'postgres'));
  for (const blocked of ['stat', 'findmnt']) {
    if (blocked === 'findmnt') write('/tmp/bin/findmnt', '#!/bin/sh\ntrap "" TERM\nsleep 120 &\nwait\n');
    const timed = performance.now();
    assert.throws(() => docker(args), (error: any) => error.status === 137);
    const duration = performance.now() - timed;
    assert.ok(duration >= 8000 && duration < 10000);
    const survivors = liveProcesses();
    assert.deepEqual(survivors.map(line => line.trim().split(/\s+/)[0]!).sort(), protectedPids, `Leaked metadata or signalled another session: ${survivors.join('; ')}`);
    shell('pg_ctl -D /scratch/source status; kill -0 "$(cat /tmp/unrelated.pid)"');
    assert.equal(shell('psql -h /scratch/socket -U postgres -d postgres -Atc "SELECT 1"').trim(), '1');
    context.diagnostic(`Same-image blocked ${blocked}: wall_ms=${duration}; only protected PG/other-session processes remain: ${survivors.join('; ')}`);
  }
});
