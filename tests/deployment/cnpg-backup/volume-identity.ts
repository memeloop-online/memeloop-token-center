import assert from 'node:assert/strict';
import { setTimeout as delay } from 'node:timers/promises';

export async function retryObservation<T>(operation: () => T | Promise<T>, options: {
  clock?: () => number; pause?: (milliseconds: number) => Promise<unknown>; report?: (attempt: number) => void;
} = {}): Promise<T> {
  const clock = options.clock ?? Date.now;
  const started = clock();
  for (let attempt = 1; ; attempt++) {
    try {
      return await operation();
    } catch (error) {
      const transient = ['API_TIMEOUT', 'API_TRANSIENT'].includes(failureCategory(error));
      if (!transient || attempt >= 3 || clock() - started >= 30_000) throw error;
      options.report?.(attempt);
      await (options.pause ?? delay)(1000);
    }
  }
}

export function failureCategory(error: unknown): string {
  const failure = (error && typeof error === 'object' ? error : {}) as { code?: string; stderr?: string | Buffer };
  if (failure.code === 'SOURCE_STAT_TIMEOUT') return 'SOURCE_STAT_TIMEOUT';
  if (failure.code === 'ERR_ASSERTION') return 'GUARD_ASSERTION';
  if (failure.code === 'ETIMEDOUT' || /context deadline exceeded/.test(String(failure.stderr ?? ''))) return 'API_TIMEOUT';
  if (/Error from server \((?:ServerTimeout|Timeout)\):/.test(String(failure.stderr ?? ''))) return 'API_TRANSIENT';
  if (/Client.Timeout|TLS handshake timeout|connection reset by peer|i\/o timeout|ServiceUnavailable|TooManyRequests/.test(String(failure.stderr ?? ''))) return 'API_TRANSIENT';
  if (error instanceof SyntaxError) return 'API_INVALID_JSON';
  return 'COLLECTOR_ERROR';
}

export function failureDetails(error: unknown): Record<string, unknown> {
  const failure = (error && typeof error === 'object' ? error : {}) as { code?: string; status?: number; signal?: string; stderr?: string | Buffer };
  return {
    category: failureCategory(error),
    exitStatus: Number.isInteger(failure.status) ? failure.status : null,
    processCode: ['ETIMEDOUT', 'ENOENT', 'EACCES', 'ENOBUFS', 'SOURCE_STAT_TIMEOUT', 'ERR_ASSERTION'].includes(failure.code ?? '') ? failure.code : null,
    signal: ['SIGKILL', 'SIGTERM', 'SIGINT', 'SIGABRT', 'SIGSEGV'].includes(failure.signal ?? '') ? failure.signal : null,
    hasStderr: typeof failure.stderr === 'string' || Buffer.isBuffer(failure.stderr),
  };
}

export const stageIdentity = {
  namespace: 'memeloop-token-center', name: 'mtc-pg-bounded-stage-20261005', node: 'haixia',
  claimUID: '95802e2f-1c75-467c-a7a4-1bfbdf2a2052',
  persistentUID: '4f5b20ed-3681-41d8-9ccb-0077d6f7daf2',
  longhornUID: 'a338e552-0247-42c1-9da6-2afa8bac3899',
  diskUUID: 'b59651cb-831f-4a8d-8438-0709359b48c8', diskPath: '/data1/longhorn',
  filesystemUUID: 'b5d8f16f-7409-444b-98a9-32812a50abe5',
  device: '/dev/longhorn/mtc-pg-bounded-stage-20261005',
  capacityGiB: 28,
  leaseSeconds: 45,
};

export type InventoryReader = (args: string[]) => string;

export function observeInventoryFailures(read: InventoryReader, report: (fields: Record<string, unknown>) => void): InventoryReader {
  return args => {
    const started = Date.now();
    try {
      return read(args);
    } catch (error) {
      const kind = args[args.indexOf('get') + 1];
      const inventoryPhases = new Map([['pod', 'pod'], ['pods', 'csi-pods'], ['pvc', 'claim'], ['pv', 'persistent-volume'], ['volumes.longhorn.io', 'longhorn-volume'], ['replicas.longhorn.io', 'longhorn-replicas']]);
      const phase = args.includes('exec')
        ? args.includes('blkid') ? 'filesystem-identity' : args.includes('stat') ? 'device-number' : 'lease-publication'
        : inventoryPhases.get(kind ?? '') ?? 'inventory-other';
      report({ phase, durationMs: Math.max(0, Date.now() - started), ...failureDetails(error) });
      throw error;
    }
  };
}

export function stageIdentityForPod(pod: any, containerName: string): typeof stageIdentity {
  const container = pod.spec.containers.find((entry: any) => entry.name === containerName);
  const value = (name: string) => container.env.find((entry: any) => entry.name === name)?.value;
  const capacity = value('REVIEWED_STAGE_CAPACITY_GIB') ?? '28';
  assert.ok(['28', '32', '40', '56'].includes(capacity), 'Unknown reviewed stage capacity profile');
  if (capacity !== '28') {
    assert.equal(value('BACKUP_MAX_BYTES'), String(Number(capacity) * 1024 ** 3));
    assert.equal(value('BACKUP_MIN_BYTES'), String((Number(capacity) - 2) * 1024 ** 3));
    assert.equal(value('HARD_CAPACITY_REVIEW_APPROVED'), 'true');
  }
  return { ...stageIdentity, capacityGiB: Number(capacity) };
}

export const archiveIdentity: typeof stageIdentity = {
  ...stageIdentity, name: 'mtc-pg-bounded-archive-20261005', node: 'versetensor-hv',
  claimUID: 'dd506d1e-0dbc-43c8-8d5f-b8138b769c76',
  persistentUID: 'fbfd4a51-af78-4cf1-9d6f-524fdb2be140',
  longhornUID: '670bac6f-4249-4513-980f-2b80569330bd',
  diskUUID: 'da83ab44-d26f-4c20-b956-87fa954cd603', diskPath: '/var/lib/longhorn/',
  filesystemUUID: 'd9b22ac2-180f-4f24-a00d-9ea3d8fd3d90',
  device: '/dev/longhorn/mtc-pg-bounded-archive-20261005',
};

export function verifyStageBinding(claim: any, persistent: any, volume: any, replicas: any[], expected = stageIdentity): void {
  for (const [resource, uid] of [[claim, expected.claimUID], [persistent, expected.persistentUID], [volume, expected.longhornUID]]) {
    assert.equal(resource.metadata.name, expected.name);
    assert.equal(resource.metadata.uid, uid, 'Reviewed NEW volume identity changed');
    assert.ok(!resource.metadata.deletionTimestamp);
  }
  assert.equal(claim.metadata.namespace, expected.namespace);
  assert.equal(claim.status.phase, 'Bound');
  assert.equal(claim.spec.volumeName, expected.name);
  assert.equal(persistent.status.phase, 'Bound');
  assert.equal(persistent.spec.claimRef.uid, expected.claimUID);
  assert.equal(persistent.spec.claimRef.name, expected.name);
  assert.equal(persistent.spec.claimRef.namespace, expected.namespace);
  assert.equal(persistent.spec.volumeMode, 'Filesystem');
  assert.equal(persistent.spec.persistentVolumeReclaimPolicy, 'Retain');
  assert.equal(persistent.spec.capacity.storage, `${expected.capacityGiB}Gi`);
  assert.equal(persistent.spec.csi.driver, 'driver.longhorn.io');
  assert.equal(persistent.spec.csi.volumeHandle, expected.name);
  assert.equal(persistent.spec.csi.fsType, 'xfs');
  assert.equal(volume.metadata.namespace, 'longhorn-system');
  assert.equal(volume.spec.size, String(expected.capacityGiB * 1024 ** 3));
  assert.equal(volume.spec.numberOfReplicas, 1);
  assert.equal(volume.spec.dataLocality, 'strict-local');
  assert.equal(volume.status.state, 'attached');
  assert.equal(volume.status.robustness, 'healthy');
  assert.equal(volume.status.currentNodeID, expected.node);
  assert.equal(replicas.length, 1, 'NEW stage volume must retain its reviewed single replica');
  const replica = replicas[0];
  assert.ok(!replica.metadata.deletionTimestamp);
  assert.equal(replica.spec.volumeName, expected.name);
  assert.equal(replica.spec.nodeID, expected.node);
  assert.equal(replica.spec.diskID, expected.diskUUID);
  assert.equal(replica.spec.diskPath, expected.diskPath);
  assert.equal(replica.spec.failedAt, '');
  assert.equal(replica.status.currentState, 'running');
}

export function attestStageVolume(read: InventoryReader, pod: any, clock: () => number = Date.now): string {
  return attestBackupVolume(read, pod, stageIdentityForPod(pod, 'export'), 'export', clock);
}

export function attestBackupVolume(read: InventoryReader, pod: any, expected: typeof stageIdentity, containerName: string, clock: () => number = Date.now, mount: 'backup' | 'scratch' = 'backup'): string {
  const observed = Math.floor(clock() / 1000);
  assert.equal(pod.metadata.namespace, expected.namespace);
  assert.equal(pod.spec.nodeName, expected.node);
  assert.match(pod.metadata.uid, /^[a-zA-Z0-9-]{1,64}$/);
  const container = pod.spec.containers.find((entry: any) => entry.name === containerName);
  const environment = (name: string) => container.env.find((entry: any) => entry.name === name);
  const environmentPrefix = mount.toUpperCase();
  assert.equal(environment(`EXPECTED_${environmentPrefix}_FS_UUID`).value, expected.filesystemUUID);
  assert.equal(environment(`EXPECTED_${environmentPrefix}_DEVICE`).value, expected.device);
  assert.equal(environment(`${environmentPrefix}_UUID_ATTESTATION`).value, 'external-csi-lease');
  assert.equal(environment('POD_UID').valueFrom.fieldRef.fieldPath, 'metadata.uid');
  const get = (args: string[]) => JSON.parse(read([...args, '-o', 'json']));
  verifyStageBinding(
    get(['-n', expected.namespace, 'get', 'pvc', expected.name]),
    get(['get', 'pv', expected.name]),
    get(['-n', 'longhorn-system', 'get', 'volumes.longhorn.io', expected.name]),
    get(['-n', 'longhorn-system', 'get', 'replicas.longhorn.io', '-l', `longhornvolume=${expected.name}`]).items,
    expected,
  );
  const plugins = get(['-n', 'longhorn-system', 'get', 'pods', '-l', 'app=longhorn-csi-plugin']).items.filter((candidate: any) =>
    candidate.spec.nodeName === expected.node && candidate.status.phase === 'Running' && !candidate.metadata.deletionTimestamp &&
    candidate.status.containerStatuses?.some((entry: any) => entry.name === 'longhorn-csi-plugin' && entry.ready));
  assert.equal(plugins.length, 1, 'Expected exactly one ready CSI plugin on the reviewed stage node');
  const prefix = ['-n', 'longhorn-system', 'exec', plugins[0].metadata.name, '-c', 'longhorn-csi-plugin', '--'];
  const identity = read([...prefix, 'blkid', '-p', '-o', 'export', expected.device]).trim().split('\n');
  assert.deepEqual(identity.filter((line: string) => line.startsWith('UUID=')), [`UUID=${expected.filesystemUUID}`]);
  assert.deepEqual(identity.filter((line: string) => line.startsWith('TYPE=')), ['TYPE=xfs']);
  const deviceNumber = read([...prefix, 'stat', '-L', '-c', '%t:%T', expected.device]).trim();
  assert.match(deviceNumber, /^[0-9a-fA-F]{1,8}:[0-9a-fA-F]{1,8}$/);
  const [major, minor] = deviceNumber.split(':').map(value => parseInt(value, 16));
  assert.ok(major! > 0);
  const age = Math.floor(clock() / 1000) - observed;
  assert.ok(age >= 0 && age <= expected.leaseSeconds, 'CSI identity observation expired before publication');
  return `${observed} ${pod.metadata.uid} ${expected.device} ${expected.filesystemUUID} ${major}:${minor}`;
}
