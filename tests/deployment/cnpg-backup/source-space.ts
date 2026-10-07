import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import { attestStageVolume, failureCategory, failureDetails, retryObservation } from './volume-identity.ts';
export { failureCategory, failureDetails, retryObservation } from './volume-identity.ts';
import { assertSourceFresh, readSourceFilesystem, sourceFilesystem } from './source-filesystem.ts';

export const sourceSpace = {
  namespace: 'memeloop-token-center', pod: 'memeloop-token-center-pg-7', node: 'haixia',
  cluster: 'memeloop-token-center-pg', claim: 'memeloop-token-center-pg-7',
  job: 'mtc-pg-bounded-stage-20261005', stageClaim: 'mtc-pg-bounded-stage-20261005',
  startBytes: 9 * 1024 ** 3, stopBytes: 8 * 1024 ** 3, maximumDropBytes: 512 * 1024 ** 2,
  minimumFreeInodes: sourceFilesystem.minimumFreeInodes, maximumSampleAgeMs: sourceFilesystem.maximumSampleAgeMs, intervalMs: 15_000,
  leaseSeconds: 45, waitSeconds: 120, deadlineMs: 43_200_000,
};


export function sourceBudget(sample: { availableBytes: number }, initialBytes?: number, report?: (fields: Record<string, unknown>) => void): void {
  const minimumBytes = initialBytes === undefined ? sourceSpace.startBytes : sourceSpace.stopBytes;
  const watermarkSatisfied = sample.availableBytes >= minimumBytes;
  const lossSatisfied = initialBytes === undefined || initialBytes - sample.availableBytes < sourceSpace.maximumDropBytes;
  report?.({
    event: 'source-budget', phase: initialBytes === undefined ? 'initial' : 'renewal',
    availableBytes: sample.availableBytes, initialAvailableBytes: initialBytes ?? null,
    minimumAvailableBytes: minimumBytes, maximumDropBytes: sourceSpace.maximumDropBytes,
    dropBytes: initialBytes === undefined ? null : initialBytes - sample.availableBytes,
    category: !watermarkSatisfied ? 'SOURCE_BUDGET_WATERMARK' : !lossSatisfied ? 'SOURCE_BUDGET_DROP' : 'SOURCE_BUDGET_OK',
  });
  assert.ok(watermarkSatisfied, 'Source space watermark breached; stop backup only');
  assert.ok(lossSatisfied, 'Source lost 512Mi since dump start; stop backup only');
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


function apiPhase(args: string[]): string {
  if (args.includes('exec')) {
    if (args.includes(sourceFilesystem.pod)) return 'source-statfs';
    if (args.includes('blkid')) return 'volume-filesystem-identity';
    if (args.includes('stat')) return 'volume-device-number';
    return args.includes('/bin/rm') ? 'lease-revoke' : 'lease-publish';
  }
  const kind = args[args.indexOf('get') + 1];
  if (kind === 'pvc' && args.includes(sourceFilesystem.claim)) return 'source-claim';
  if (kind === 'pv' && args.includes(sourceFilesystem.persistent)) return 'source-pv';
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
      emit(`${kind}-end`, { ...identity, durationMs: performance.now() - started, outcome: 'failure', ...failureDetails(error) });
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
  const kubectl = (args: string[], timeoutMs = 20_000) => measured('api', apiPhase(args), () => {
    try {
      return execFileSync('kubectl', [timeoutMs === sourceFilesystem.processTimeoutMs ? '--request-timeout=8s' : '--request-timeout=15s', ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout: timeoutMs, killSignal: 'SIGKILL', maxBuffer: 8 * 1024 ** 2 });
    } catch (error) {
      if (apiPhase(args) === 'source-statfs' && [124, 137].includes((error as { status: number }).status)) throw Object.assign(new Error('Source stat timed out'), { code: 'SOURCE_STAT_TIMEOUT' });
      throw error;
    }
  });
  const get = (kind: string, name: string) => JSON.parse(kubectl(['-n', sourceSpace.namespace, 'get', kind, name, '-o', 'json']));
  const observeSource = (fields: Record<string, unknown>) => emit(typeof fields.event === 'string' ? fields.event : 'source-sample', fields);
  const read = () => readSourceFilesystem(kubectl, observeSource);
  const report = (retryAttempt: number) => emit('transient-api-retry-no-lease-renewal', { retryAttempt });
  const initial = await retryObservation(() => collect('initial-source', read), { report });
  sourceBudget(initial, undefined, observeSource);
  if (mode === '--check') {
    console.log(JSON.stringify({ ...initial, sourceSpace, operation: 'read-only-no-source-sql-no-pvc-mount' }, null, 2));
  } else {
    let watchedUID: string | undefined;
    const started = Date.now();
    try {
      while (Date.now() - started < sourceSpace.deadlineMs) {
        const cycleStarted = performance.now();
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
          assert.deepEqual(current.sourceIdentity, initial.sourceIdentity, 'Source container or binding changed since start');
          assert.equal(current.mount, initial.mount, 'Source mount changed since start');
          assert.equal(current.filesystemId, initial.filesystemId, 'Source filesystem changed since start');
          sourceBudget(current, initial.availableBytes, observeSource);
          assertSourceFresh(current, observeSource);
          const epoch = Math.floor(Date.now() / 1000);
          assert.ok(epoch - Number(volumeLease.split(' ')[0]) <= 45, 'CSI observation expired while sampling source space');
          const leaseTiming = () => ({ sourceLeaseEpoch: epoch, volumeLeaseEpoch: Number(volumeLease.split(' ')[0]), sourceSampleAt: current.time, sampleAgeMs: Date.now() - Date.parse(current.time), leaseSeconds: sourceSpace.leaseSeconds });
          emit('lease-publication-start', leaseTiming());
          kubectl(['-n', sourceSpace.namespace, 'exec', exportName!, '-c', 'export', '--', '/bin/sh', '-ec', `umask 077; printf '%s\\n' '${volumeLease}' > /tmp/backup-volume.lease.partial; mv /tmp/backup-volume.lease.partial /tmp/backup-volume.lease; printf '%s\\n' ${epoch} > /tmp/source-space.lease.partial; mv /tmp/source-space.lease.partial /tmp/source-space.lease`]);
          assertSourceFresh(current, observeSource);
          emit('lease-publication-end', leaseTiming());
          console.log(JSON.stringify({ observedAt: new Date().toISOString(), ...current, initialAvailableBytes: initial.availableBytes }));
          return true;
        }), { report });
        if (!observation) break;
        const cycleElapsedMs = performance.now() - cycleStarted;
        const delayMs = Math.max(0, sourceSpace.intervalMs - cycleElapsedMs);
        emit('renewal-schedule', { cycleElapsedMs, delayMs });
        await delay(delayMs);
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
