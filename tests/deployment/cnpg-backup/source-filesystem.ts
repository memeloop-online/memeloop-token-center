import assert from 'node:assert/strict';
import { performance } from 'node:perf_hooks';

export const sourceFilesystem = {
  namespace: 'memeloop-token-center', pod: 'memeloop-token-center-pg-7', container: 'postgres', node: 'haixia',
  cluster: 'memeloop-token-center-pg', claim: 'memeloop-token-center-pg-7',
  podUID: '349e6202-7073-4166-ad65-323ada589506', claimUID: '74b34d6c-c730-4b9f-bfdc-2f165727c40a',
  persistent: 'pvc-74b34d6c-c730-4b9f-bfdc-2f165727c40a', persistentUID: '354d4ce8-235f-4a72-a24b-ceaac8b2ce5f',
  pgdata: '/var/lib/postgresql/data/pgdata', mount: '/var/lib/postgresql/data', capacityBytes: 30 * 1024 ** 3,
  maximumSampleAgeMs: 90_000, minimumFreeInodes: 65536, processTimeoutMs: 10_000,
};

export const sourceStatScript = `test "$PGDATA" = ${sourceFilesystem.pgdata}
before=$(findmnt -rn -o TARGET,SOURCE,FSTYPE,FSROOT,MAJ:MIN -T "$PGDATA")
set -- $before
test "$#" -eq 5
test "$1" = ${sourceFilesystem.mount}
test "$2" = /dev/longhorn/${sourceFilesystem.persistent}
test "$3" = xfs
test "$4" = /
printf 'mount_before=%s\\n' "$before"
stat -f -c 'statfs=%S %b %f %a %c %d %T %i' "$PGDATA"
after=$(findmnt -rn -o TARGET,SOURCE,FSTYPE,FSROOT,MAJ:MIN -T "$PGDATA")
test "$before" = "$after"
printf 'mount_after=%s\\n' "$after"`;

export const sourceStatCommand = ['setsid', '--fork', '--wait', '/bin/sh', '-c', 'timeout --signal=KILL 8s /bin/sh -ec "$1"; status=$?; exit "$status"', 'source-statfs', sourceStatScript];

export function verifySourceBinding(pod: any, claim: any, persistent: any, cluster: any) {
  const expected = sourceFilesystem;
  for (const [resource, name, uid] of [[pod, expected.pod, expected.podUID], [claim, expected.claim, expected.claimUID], [persistent, expected.persistent, expected.persistentUID]]) {
    assert.equal(resource.metadata.name, name);
    assert.equal(resource.metadata.uid, uid, 'Source identity changed');
    assert.ok(!resource.metadata.deletionTimestamp);
  }
  assert.equal(pod.metadata.namespace, expected.namespace);
  assert.equal(claim.metadata.namespace, expected.namespace);
  assert.equal(cluster.metadata.name, expected.cluster);
  assert.equal(cluster.metadata.namespace, expected.namespace);
  assert.equal(cluster.spec.instances, 1);
  assert.equal(cluster.status.currentPrimary, expected.pod);
  assert.equal(pod.spec.nodeName, expected.node);
  assert.equal(pod.status.phase, 'Running');
  const containers = pod.spec.containers.filter((container: any) => container.name === expected.container);
  const statuses = pod.status.containerStatuses.filter((container: any) => container.name === expected.container);
  assert.equal(containers.length, 1);
  assert.equal(statuses.length, 1);
  const container = containers[0];
  const status = statuses[0];
  assert.equal(status.ready, true);
  assert.ok(status.state.running);
  assert.match(status.containerID, /^containerd:\/\/[a-f0-9]{64}$/);
  assert.ok(Number.isSafeInteger(status.restartCount) && status.restartCount >= 0);
  assert.deepEqual(container.env.filter((entry: any) => entry.name === 'PGDATA'), [{ name: 'PGDATA', value: expected.pgdata }]);
  assert.deepEqual(container.volumeMounts.filter((mount: any) => mount.name === 'pgdata'), [{ name: 'pgdata', mountPath: expected.mount }]);
  assert.equal(container.volumeMounts.filter((mount: any) => expected.pgdata === mount.mountPath || expected.pgdata.startsWith(`${mount.mountPath}/`) || mount.mountPath.startsWith(`${expected.pgdata}/`)).length, 1);
  assert.deepEqual(pod.spec.volumes.filter((volume: any) => volume.name === 'pgdata'), [{ name: 'pgdata', persistentVolumeClaim: { claimName: expected.claim } }]);
  assert.equal(claim.status.phase, 'Bound');
  assert.equal(claim.spec.volumeName, expected.persistent);
  assert.equal(claim.spec.volumeMode, 'Filesystem');
  assert.equal(persistent.status.phase, 'Bound');
  assert.equal(persistent.spec.volumeMode, 'Filesystem');
  const storage = persistent.spec.capacity.storage;
  assert.match(storage, /^[1-9][0-9]*Gi$/);
  const capacityBytes = Number(storage.slice(0, -2)) * 1024 ** 3;
  assert.ok(Number.isSafeInteger(capacityBytes) && capacityBytes >= expected.capacityBytes);
  assert.equal(cluster.spec.storage.size, storage);
  assert.equal(claim.spec.resources.requests.storage, storage);
  assert.equal(claim.status.capacity.storage, storage);
  assert.ok(!claim.status.conditions?.some((condition: any) => condition.status === 'True'));
  assert.equal(persistent.spec.claimRef.uid, expected.claimUID);
  assert.equal(persistent.spec.claimRef.name, expected.claim);
  assert.equal(persistent.spec.claimRef.namespace, expected.namespace);
  assert.equal(persistent.spec.csi.driver, 'driver.longhorn.io');
  assert.equal(persistent.spec.csi.volumeHandle, expected.persistent);
  assert.equal(persistent.spec.csi.fsType, 'xfs');
  assert.deepEqual(persistent.spec.nodeAffinity.required.nodeSelectorTerms, [{ matchExpressions: [{ key: 'kubernetes.io/hostname', operator: 'In', values: [expected.node] }] }]);
  return { podUID: pod.metadata.uid, containerID: status.containerID, restarts: status.restartCount, claimUID: claim.metadata.uid, persistentUID: persistent.metadata.uid, node: pod.spec.nodeName, capacityBytes };
}

export function parseSourceStat(output: string, expectedCapacityBytes = sourceFilesystem.capacityBytes) {
  assert.ok(Number.isSafeInteger(expectedCapacityBytes) && expectedCapacityBytes >= sourceFilesystem.capacityBytes);
  assert.ok(output.length <= 2048, 'Source stat output exceeds bounded metadata');
  const lines = output.trim().split('\n');
  assert.equal(lines.length, 3);
  assert.ok(lines[0]!.startsWith('mount_before=') && lines[2]!.startsWith('mount_after='));
  const mount = lines[0]!.slice('mount_before='.length);
  assert.equal(mount, lines[2]!.slice('mount_after='.length), 'Source mount changed during stat');
  const fields = mount.split(/\s+/);
  assert.equal(fields.length, 5);
  assert.deepEqual(fields.slice(0, 4), [sourceFilesystem.mount, `/dev/longhorn/${sourceFilesystem.persistent}`, 'xfs', '/']);
  assert.match(fields[4]!, /^[1-9][0-9]*:[0-9]+$/);
  assert.ok(lines[1]!.startsWith('statfs='));
  const values = lines[1]!.slice('statfs='.length).split(' ');
  assert.equal(values.length, 8);
  assert.equal(values[6], 'xfs');
  assert.match(values[7]!, /^[a-fA-F0-9]+$/);
  for (const value of values.slice(0, 6)) assert.match(value, /^[0-9]+$/);
  const [blockSize, totalBlocks, freeBlocks, availableBlocks, totalInodes, freeInodes] = values.slice(0, 6).map(Number) as [number, number, number, number, number, number];
  assert.ok([blockSize, totalBlocks, freeBlocks, availableBlocks, totalInodes, freeInodes].every(Number.isSafeInteger));
  assert.ok(blockSize > 0 && totalBlocks > 0 && availableBlocks <= freeBlocks && freeBlocks <= totalBlocks);
  assert.ok(totalInodes > 0 && freeInodes <= totalInodes && freeInodes >= sourceFilesystem.minimumFreeInodes);
  const capacityBytes = blockSize * totalBlocks;
  const availableBytes = blockSize * availableBlocks;
  assert.ok(Number.isSafeInteger(capacityBytes) && capacityBytes <= expectedCapacityBytes);
  if (expectedCapacityBytes > sourceFilesystem.capacityBytes) assert.ok(capacityBytes >= expectedCapacityBytes - 2 * 1024 ** 3, 'Expanded source filesystem has not reached the reconciled capacity');
  assert.ok(Number.isSafeInteger(availableBytes) && availableBytes >= 0);
  return { capacityBytes, availableBytes, freeInodes, mount, filesystemId: values[7]! };
}

type Clock = { wall: () => number; monotonic: () => number };
type Observer = (fields: Record<string, unknown>) => void;
const clock: Clock = { wall: Date.now, monotonic: () => performance.now() };

export function assertSourceFresh(sample: { startedWallMs: number; startedMonotonicMs: number }, observe: Observer = () => {}, timer: Clock = clock): void {
  const now = timer.wall();
  const wallAge = now - sample.startedWallMs;
  const monotonicAge = timer.monotonic() - sample.startedMonotonicMs;
  const category = !Number.isFinite(wallAge) || !Number.isFinite(monotonicAge) ? 'SOURCE_SAMPLE_INVALID'
    : wallAge < -5000 ? 'SOURCE_SAMPLE_FUTURE'
    : monotonicAge < 0 || Math.abs(wallAge - monotonicAge) > 5000 ? 'SOURCE_CLOCK_INCOMPATIBLE'
    : Math.max(wallAge, monotonicAge) > sourceFilesystem.maximumSampleAgeMs ? 'SOURCE_SAMPLE_STALE' : 'SOURCE_SAMPLE_FRESH';
  observe({ category, method: 'mounted-statfs', sourceSampleAt: Number.isFinite(sample.startedWallMs) ? new Date(sample.startedWallMs).toISOString() : null, sampleAgeMs: Number.isFinite(wallAge) ? wallAge : null, monotonicAgeMs: Number.isFinite(monotonicAge) ? monotonicAge : null, validatedAt: new Date(now).toISOString() });
  assert.equal(category, 'SOURCE_SAMPLE_FRESH', 'Source stat observation expired or clock incompatible');
}

export function readSourceFilesystem(read: (args: string[], timeoutMs?: number) => string, observe: Observer = () => {}, timer: Clock = clock) {
  const expected = sourceFilesystem;
  const startedWallMs = timer.wall();
  const startedMonotonicMs = timer.monotonic();
  const get = (kind: string, name: string) => JSON.parse(read([...(kind === 'pv' ? [] : ['-n', expected.namespace]), 'get', kind, name, '-o', 'json']));
  const binding = (phase: 'before' | 'after') => {
    const cluster = get('clusters.postgresql.cnpg.io', expected.cluster);
    const pod = phase === 'before' ? get('pod', expected.pod) : undefined;
    const claim = get('pvc', expected.claim);
    const persistent = get('pv', expected.persistent);
    const identity = verifySourceBinding(pod ?? get('pod', expected.pod), claim, persistent, cluster);
    observe({ event: 'source-identity', phase, ...identity });
    return identity;
  };
  const before = binding('before');
  const execStartedAt = new Date(timer.wall()).toISOString();
  const output = read(['-n', expected.namespace, 'exec', expected.pod, '-c', expected.container, '--', ...sourceStatCommand], expected.processTimeoutMs);
  const execCompletedAt = new Date(timer.wall()).toISOString();
  const counters = parseSourceStat(output, before.capacityBytes);
  const after = binding('after');
  assert.deepEqual(after, before, 'Source identity changed across stat; discard sample');
  const sample = { ...counters, method: 'mounted-statfs', time: new Date(startedWallMs).toISOString(), startedWallMs, startedMonotonicMs, execStartedAt, execCompletedAt, collectedAt: new Date(timer.wall()).toISOString(), sourcePodUID: before.podUID, sourceIdentity: before };
  assertSourceFresh(sample, observe, timer);
  return sample;
}
