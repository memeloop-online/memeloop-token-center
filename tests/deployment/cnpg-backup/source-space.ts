import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import { attestStageVolume } from './volume-identity.ts';

export const sourceSpace = {
  namespace: 'memeloop-token-center', pod: 'memeloop-token-center-pg-7', node: 'haixia',
  cluster: 'memeloop-token-center-pg', claim: 'memeloop-token-center-pg-7',
  job: 'mtc-pg-bounded-stage-20261005', stageClaim: 'mtc-pg-bounded-stage-20261005',
  startBytes: 9 * 1024 ** 3, stopBytes: 8 * 1024 ** 3, maximumDropBytes: 512 * 1024 ** 2,
  minimumFreeInodes: 65536, maximumSampleAgeMs: 90_000, intervalMs: 15_000,
  leaseSeconds: 45, waitSeconds: 120, deadlineMs: 43_200_000,
};

export async function retryObservation<T>(operation: () => T | Promise<T>, options: {
  clock?: () => number; pause?: (milliseconds: number) => Promise<unknown>; report?: (attempt: number) => void;
} = {}): Promise<T> {
  const clock = options.clock ?? Date.now;
  const started = clock();
  for (let attempt = 1; ; attempt++) {
    try {
      return await operation();
    } catch (error) {
      const failure = error as { code?: string; stderr?: string | Buffer; message?: string };
      const transient = failure.code === 'ETIMEDOUT' || /Client.Timeout|TLS handshake timeout|connection reset by peer|i\/o timeout|ServiceUnavailable|TooManyRequests/.test(String(failure.stderr ?? ''));
      if (!transient || attempt >= 3 || clock() - started >= 30_000) throw error;
      options.report?.(attempt);
      await (options.pause ?? delay)(1000);
    }
  }
}

export function sourceSample(summary: any, now = Date.now(), observe?: (fields: Record<string, unknown>) => void): { availableBytes: number; time: string; freeInodes: number } {
  assert.equal(summary.node.nodeName, sourceSpace.node);
  const pods = summary.pods.filter((pod: any) => pod.podRef.namespace === sourceSpace.namespace && pod.podRef.name === sourceSpace.pod);
  assert.equal(pods.length, 1, 'Source pod stats missing or ambiguous');
  const volumes = pods[0].volume.filter((volume: any) => volume.name === 'pgdata' && volume.pvcRef?.name === sourceSpace.claim && volume.pvcRef?.namespace === sourceSpace.namespace);
  assert.equal(volumes.length, 1, 'Source pgdata stats missing or ambiguous');
  const volume = volumes[0];
  const timestamp = Date.parse(volume.time);
  const age = now - timestamp;
  const category = !Number.isFinite(age) ? 'SOURCE_SAMPLE_INVALID' : age < -5000 ? 'SOURCE_SAMPLE_FUTURE' : age > sourceSpace.maximumSampleAgeMs ? 'SOURCE_SAMPLE_STALE' : 'SOURCE_SAMPLE_FRESH';
  observe?.({ category, sourceSampleAt: Number.isFinite(timestamp) ? new Date(timestamp).toISOString() : null, sampleAgeMs: Number.isFinite(age) ? age : null, validatedAt: new Date(now).toISOString() });
  assert.equal(category, 'SOURCE_SAMPLE_FRESH', 'Source sample invalid, future or stale; revoke dump lease');
  assert.ok(Number.isSafeInteger(volume.availableBytes) && volume.availableBytes >= 0 && volume.availableBytes <= volume.capacityBytes);
  assert.ok(Number.isSafeInteger(volume.inodesFree) && volume.inodesFree >= sourceSpace.minimumFreeInodes, 'Source inode reserve breached');
  return { availableBytes: volume.availableBytes, time: new Date(timestamp).toISOString(), freeInodes: volume.inodesFree };
}

export function sourceBudget(sample: { availableBytes: number }, initialBytes?: number): void {
  assert.ok(sample.availableBytes >= (initialBytes === undefined ? sourceSpace.startBytes : sourceSpace.stopBytes), 'Source space watermark breached; stop backup only');
  if (initialBytes !== undefined) assert.ok(initialBytes - sample.availableBytes < sourceSpace.maximumDropBytes, 'Source lost 512Mi since dump start; stop backup only');
}

export function exportPod(pod: any): void {
  assert.equal(pod.metadata.namespace, sourceSpace.namespace);
  assert.equal(pod.metadata.labels['job-name'], sourceSpace.job);
  assert.ok(pod.metadata.ownerReferences.some((owner: any) => owner.kind === 'Job' && owner.name === sourceSpace.job));
  assert.equal(pod.spec.nodeName, sourceSpace.node);
  assert.ok(!pod.spec.volumes.some((volume: any) => volume.hostPath));
  assert.deepEqual(pod.spec.volumes.filter((volume: any) => volume.persistentVolumeClaim).map((volume: any) => volume.persistentVolumeClaim.claimName), [sourceSpace.stageClaim]);
  assert.ok(pod.spec.containers.some((container: any) => container.name === 'export'));
}

function failureCategory(error: unknown): string {
  const failure = error as { code?: string; stderr?: string | Buffer };
  if (failure.code === 'ETIMEDOUT') return 'API_TIMEOUT';
  if (/Client.Timeout|TLS handshake timeout|connection reset by peer|i\/o timeout|ServiceUnavailable|TooManyRequests/.test(String(failure.stderr ?? ''))) return 'API_TRANSIENT';
  if (failure.code === 'ERR_ASSERTION') return 'GUARD_ASSERTION';
  if (error instanceof SyntaxError) return 'API_INVALID_JSON';
  return 'COLLECTOR_ERROR';
}

function apiPhase(args: string[]): string {
  if (args.includes('exec')) {
    if (args.includes('blkid')) return 'volume-filesystem-identity';
    if (args.includes('stat')) return 'volume-device-number';
    return args.includes('/bin/rm') ? 'lease-revoke' : 'lease-publish';
  }
  if (args.includes('--raw')) return 'source-summary';
  const kind = args[args.indexOf('get') + 1];
  if (kind === 'pod') return args.includes(sourceSpace.pod) ? 'source-pod' : 'export-pod';
  return ({ 'clusters.postgresql.cnpg.io': 'source-cluster', pvc: 'volume-claim', pv: 'volume-pv', 'volumes.longhorn.io': 'volume-longhorn', 'replicas.longhorn.io': 'volume-replicas', pods: 'volume-csi-pods' } as Record<string, string>)[kind!] ?? 'api-other';
}

async function main(): Promise<void> {
  let cycle = 0;
  let attempt = 0;
  let call = 0;
  const emit = (event: string, fields: Record<string, unknown> = {}) => console.error(JSON.stringify({ event, observedAt: new Date().toISOString(), monotonicMs: performance.now(), cycle, attempt, ...fields }));
  const measured = <T>(kind: string, phase: string, operation: () => T): T => {
    const identity = { phase, ...(kind === 'api' ? { call: ++call } : {}) };
    const started = performance.now();
    emit(`${kind}-start`, identity);
    try {
      const result = operation();
      emit(`${kind}-end`, { ...identity, durationMs: performance.now() - started, outcome: 'success' });
      return result;
    } catch (error) {
      emit(`${kind}-end`, { ...identity, durationMs: performance.now() - started, outcome: 'failure', category: failureCategory(error) });
      throw error;
    }
  };
  const collect = <T>(phase: string, operation: () => T): T => {
    attempt++;
    return measured('collection', phase, operation);
  };
  const [mode, exportName] = process.argv.slice(2);
  assert.ok((mode === '--check' && process.argv.length === 3) || (mode === '--watch' && process.argv.length === 4));
  if (mode === '--watch') {
    assert.equal(process.env.PARENT_REVIEW_APPROVED, 'true', 'Watcher writes only an approved export pod tmpfs lease');
    assert.match(exportName!, /^mtc-pg-bounded-stage-20261005-[a-z0-9-]+$/);
  }
  const kubectl = (args: string[]) => measured('api', apiPhase(args), () => execFileSync('kubectl', ['--request-timeout=15s', ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout: 20_000, killSignal: 'SIGKILL', maxBuffer: 8 * 1024 ** 2 }));
  const get = (kind: string, name: string) => JSON.parse(kubectl(['-n', sourceSpace.namespace, 'get', kind, name, '-o', 'json']));
  const read = () => {
    const cluster = get('clusters.postgresql.cnpg.io', sourceSpace.cluster);
    assert.equal(cluster.status.currentPrimary, sourceSpace.pod);
    const pod = get('pod', sourceSpace.pod);
    assert.equal(pod.spec.nodeName, sourceSpace.node);
    assert.ok(pod.status.containerStatuses.some((container: any) => container.name === 'postgres' && container.ready));
    const summary = JSON.parse(kubectl(['get', '--raw', `/api/v1/nodes/${sourceSpace.node}/proxy/stats/summary`]));
    assert.equal(summary.pods.find((entry: any) => entry.podRef.namespace === sourceSpace.namespace && entry.podRef.name === sourceSpace.pod)?.podRef.uid, pod.metadata.uid, 'Source stats belong to a replaced pod');
    return { ...sourceSample(summary, Date.now(), fields => emit('source-sample', fields)), sourcePodUID: pod.metadata.uid };
  };
  const report = (retryAttempt: number) => emit('transient-api-retry-no-lease-renewal', { retryAttempt });
  const initial = await retryObservation(() => collect('initial-source', read), { report });
  sourceBudget(initial);
  if (mode === '--check') {
    console.log(JSON.stringify({ ...initial, sourceSpace, operation: 'read-only-no-source-sql-no-pvc-mount' }, null, 2));
  } else {
    let watchedUID: string | undefined;
    const started = Date.now();
    try {
      while (Date.now() - started < sourceSpace.deadlineMs) {
        cycle++;
        attempt = 0;
        const observation = await retryObservation(() => collect('renewal', () => {
          const pod = get('pod', exportName!);
          exportPod(pod);
          watchedUID ??= pod.metadata.uid;
          assert.equal(pod.metadata.uid, watchedUID, 'Export pod replaced; refuse to write a new pod');
          const terminated = pod.status.containerStatuses?.find((container: any) => container.name === 'export')?.state.terminated;
          if (terminated) {
            assert.equal(terminated.exitCode, 0, 'Export failed; partial is not a backup');
            console.log('Export process completed; offhost copy and isolated restore remain required');
            return false;
          }
          assert.equal(pod.status.phase, 'Running');
          const volumeLease = attestStageVolume(kubectl, pod);
          const current = read();
          assert.equal(current.sourcePodUID, initial.sourcePodUID, 'Source pod replaced; stop backup');
          sourceBudget(current, initial.availableBytes);
          const epoch = Math.floor(Date.now() / 1000);
          assert.ok(epoch - Number(volumeLease.split(' ')[0]) <= 45, 'CSI observation expired while sampling source space');
          const leaseTiming = () => ({ sourceLeaseEpoch: epoch, volumeLeaseEpoch: Number(volumeLease.split(' ')[0]), sourceSampleAt: current.time, sampleAgeMs: Date.now() - Date.parse(current.time), leaseSeconds: sourceSpace.leaseSeconds });
          emit('lease-publication-start', leaseTiming());
          kubectl(['-n', sourceSpace.namespace, 'exec', exportName!, '-c', 'export', '--', '/bin/sh', '-ec', `umask 077; printf '%s\\n' '${volumeLease}' > /tmp/backup-volume.lease.partial; mv /tmp/backup-volume.lease.partial /tmp/backup-volume.lease; printf '%s\\n' ${epoch} > /tmp/source-space.lease.partial; mv /tmp/source-space.lease.partial /tmp/source-space.lease`]);
          emit('lease-publication-end', leaseTiming());
          console.log(JSON.stringify({ observedAt: new Date().toISOString(), ...current, initialAvailableBytes: initial.availableBytes }));
          return true;
        }), { report });
        if (!observation) break;
        await delay(sourceSpace.intervalMs);
      }
      assert.ok(Date.now() - started < sourceSpace.deadlineMs, 'Source watcher deadline exceeded');
    } catch (error) {
      emit('watcher-stopping-renewal', { leaseSeconds: sourceSpace.leaseSeconds, category: failureCategory(error) });
      if (watchedUID) {
        try {
          const pod = get('pod', exportName!);
          exportPod(pod);
          if (pod.metadata.uid === watchedUID) kubectl(['-n', sourceSpace.namespace, 'exec', exportName!, '-c', 'export', '--', '/bin/rm', '-f', '/tmp/source-space.lease', '/tmp/backup-volume.lease']);
        } catch (revokeError) { emit('lease-revoke-failed', { category: failureCategory(revokeError) }); }
      }
      throw error;
    }
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => {
    console.error(JSON.stringify({ event: 'collector-failed', observedAt: new Date().toISOString(), monotonicMs: performance.now(), category: failureCategory(error) }));
    process.exitCode = 1;
  });
}
