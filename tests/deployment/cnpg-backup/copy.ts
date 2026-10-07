import assert from 'node:assert/strict';
import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { copyIdentities, copySides, validateCopyPod } from './copy-guard.ts';

export const archiveDirectory = '/backup/mtc-pg-logical-20261004';
export const archiveName = 'memeloop_token_center.dump';
export const archiveLimit = 25_769_803_776;
export const copyChunk = 4 * 1024 * 1024;
export type Remote = (side: 'source' | 'destination', command: string[], input?: Buffer) => Buffer;
const copyStages = ['capacity', 'source_checksum', 'source_metadata', 'destination_directory', 'destination_markers', 'destination_offset', 'destination_space', 'source_chunk', 'destination_chunk', 'destination_finalize'] as const;
const copyCheckpoints = ['lease_status', 'pod_read', 'pod_identity', 'pod_spec', 'container_identity', 'exec'] as const;
export type CopyStep = { stage: typeof copyStages[number]; side: 'source' | 'destination'; checkpoint?: typeof copyCheckpoints[number] };

export function copyFailureReceipt(reason: unknown, step?: CopyStep) {
  const error = reason && typeof reason === 'object' ? reason as Record<string, unknown> : {};
  const knownCodes = ['ERR_ASSERTION', 'ETIMEDOUT', 'ENOBUFS', 'EPIPE', 'ECONNRESET', 'ENOENT', 'EACCES'];
  return {
    event: 'copy_failed',
    stage: step && copyStages.includes(step.stage) ? step.stage : 'preparation',
    side: step && ['source', 'destination'].includes(step.side) ? step.side : null,
    checkpoint: step?.checkpoint && copyCheckpoints.includes(step.checkpoint) ? step.checkpoint : null,
    kind: reason instanceof assert.AssertionError ? 'assertion' : reason instanceof Error ? 'error' : 'unknown',
    code: typeof error.code === 'string' && knownCodes.includes(error.code) ? error.code : null,
    exit_code: Number.isSafeInteger(error.status) ? error.status : null,
    signal: typeof error.signal === 'string' && ['SIGTERM', 'SIGKILL', 'SIGINT'].includes(error.signal) ? error.signal : null,
    stdout_bytes: Buffer.isBuffer(error.stdout) ? error.stdout.length : null,
    stderr_bytes: Buffer.isBuffer(error.stderr) ? error.stderr.length : null,
    archive_verified: false,
    automatic_retry: false,
  };
}

export async function copyArchive(remote: Remote, expectedSha: string, pause: (milliseconds: number) => Promise<void> = async milliseconds => { await delay(milliseconds); }, hardCapacity = false, directory = archiveDirectory, rateMiB = 1, observe: (step: CopyStep) => void = () => {}): Promise<void> {
  assert.ok([1, 4, 8].includes(rateMiB), 'Copy rate must be 1, 4 or 8 MiB/s');
  assert.match(expectedSha, /^[0-9a-f]{64}$/);
  assert.match(directory, /^\/backup\/mtc-pg-logical-[0-9]{8}(?:-[a-z0-9]+)*$/);
  const run = (stage: CopyStep['stage'], side: CopyStep['side'], command: string[], input?: Buffer) => {
    observe({ stage, side });
    return remote(side, command, input);
  };
  const capacityCheck = (side: 'source' | 'destination') => {
    if (hardCapacity) run('capacity', side, ['/bin/sh', '-ec', '. /policy/capacity.sh; capacity_backup']);
  };
  capacityCheck('source');
  capacityCheck('destination');
  const shell = (stage: CopyStep['stage'], side: CopyStep['side'], body: string, input?: Buffer) =>
    run(stage, side, ['/bin/sh', '-ec', `umask 077; cd ${directory}; ${body}`], input);
  const archive = archiveName;
  shell('source_checksum', 'source', `test -f LOCAL_ARCHIVE_CREATED; printf '%s  ${archive}\n' '${expectedSha}' | sha256sum -c - >/dev/null`);
  const size = Number(shell('source_metadata', 'source', `stat -c %s ${archive}`).toString().trim());
  assert.ok(Number.isSafeInteger(size) && size > 0 && size <= archiveLimit, 'Source size exceeds bounded archive contract');
  run('destination_directory', 'destination', ['/bin/sh', '-ec', `umask 077; mkdir -p ${directory}`]);
  if (hardCapacity) shell('destination_markers', 'destination', 'test ! -e LOCAL_ARCHIVE_CREATED; test ! -e OFFHOST_COPY_VERIFIED');
  const offset = Number(shell('destination_offset', 'destination', `test ! -e ${archive}; if test -e ${archive}.partial; then stat -c %s ${archive}.partial; else printf 0; fi`).toString().trim());
  assert.ok(Number.isSafeInteger(offset) && offset >= 0 && offset <= size, 'Invalid resume offset');
  for (let position = Math.floor(offset / copyChunk) * copyChunk; position < size; position += copyChunk) {
    capacityCheck('source');
    capacityCheck('destination');
    const availableKiB = Number(shell('destination_space', 'destination', "timeout 5 df -Pk . | awk 'NR == 2 { print $4 }'").toString().trim());
    const reserveKiB = hardCapacity ? 266_240 : 33_558_528;
    assert.ok(Number.isFinite(availableKiB) && availableKiB >= reserveKiB, 'Destination must retain its reviewed reserve plus one chunk; preserve partial');
    const chunk = shell('source_chunk', 'source', `dd if=${archive} bs=4194304 skip=${position / copyChunk} count=1 iflag=fullblock status=none`);
    assert.equal(chunk.length, Math.min(copyChunk, size - position), 'Short source read');
    shell('destination_chunk', 'destination', `prlimit --core=0:0 --fsize=25769803776:25769803776 -- dd of=${archive}.partial bs=4194304 seek=${position / copyChunk} count=1 iflag=fullblock conv=notrunc,fsync status=none`, chunk);
    await pause(chunk.length / 1024 / 1024 / rateMiB * 1000);
  }
  capacityCheck('destination');
  const markerTrap = hardCapacity ? `trap 'marker_status=$?; if test "$marker_status" -ne 0; then rm -f LOCAL_ARCHIVE_CREATED OFFHOST_COPY_VERIFIED; fi; exit "$marker_status"' EXIT; ` : '';
  const receiptGuard = hardCapacity ? '. /policy/capacity.sh; capacity_backup; ' : '';
  shell('destination_finalize', 'destination', `${markerTrap}test "$(stat -c %s ${archive}.partial)" -eq ${size}; printf '%s  ${archive}.partial\n' '${expectedSha}' | sha256sum -c - >/dev/null; prlimit --core=0:0 --fsize=16777216:16777216 -- pg_restore --list ${archive}.partial > ${archive}.list.partial; test -s ${archive}.list.partial; ${receiptGuard}mv ${archive}.partial ${archive}; mv ${archive}.list.partial ${archive}.list; printf '%s  ${archive}\n' '${expectedSha}' > ${archive}.sha256; sync -f .; ${receiptGuard}touch LOCAL_ARCHIVE_CREATED OFFHOST_COPY_VERIFIED; sync -f .; ${hardCapacity ? 'capacity_backup' : ':'}`);
}

async function main(observe: (step: CopyStep) => void): Promise<void> {
  assert.equal(process.env.PARENT_REVIEW_APPROVED, 'true', 'Parent review required; this command never starts a dump');
  const configuredRate = process.env.BACKUP_COPY_RATE_MIB_PER_SECOND ?? '1';
  assert.ok(['1', '4', '8'].includes(configuredRate), 'Copy rate requires an explicit reviewed 1, 4 or 8 MiB/s selection');
  const [sourcePod, destinationPod, expectedSha] = process.argv.slice(2);
  assert.ok(sourcePod && destinationPod && expectedSha, 'Usage: copy.ts SOURCE_COPY_POD DESTINATION_COPY_POD SOURCE_SHA256');
  assert.equal(process.argv.length, 5);
  assert.match(expectedSha, /^[0-9a-f]{64}$/);
  const pods = { source: sourcePod, destination: destinationPod };
  const identities: Record<string, any> = {};
  const getPod = (name: string) => JSON.parse(execFileSync('kubectl', ['--request-timeout=8s', '-n', 'memeloop-token-center', 'get', 'pod', name, '-o', 'json'], { encoding: 'utf8', timeout: 10_000, stdio: ['ignore', 'pipe', 'pipe'] }));
  for (const side of copySides) {
    const pod = getPod(pods[side]);
    validateCopyPod(pod, side);
    identities[side] = pod;
  }
  const directory = process.env.BACKUP_ARCHIVE_DIRECTORY ?? archiveDirectory;
  assert.match(directory, /^\/backup\/mtc-pg-logical-[0-9]{8}(?:-[a-z0-9]+)*$/);
  const guards: ChildProcess[] = [];
  try {
    await Promise.all(copySides.map(side => new Promise<void>((resolve, reject) => {
      const guard = spawn(process.execPath, [fileURLToPath(new URL('./copy-guard.ts', import.meta.url)), side, pods[side], identities[side].metadata.uid], { stdio: ['ignore', 'pipe', 'pipe', 'ipc'] });
      guards.push(guard);
      guard.stderr!.pipe(process.stderr);
      const timer = setTimeout(() => reject(new Error('Copy lease startup timeout')), 110_000);
      let output = '';
      guard.stdout!.on('data', chunk => { output += chunk.toString(); if (output === 'COPY_LEASES_READY\n') { clearTimeout(timer); resolve(); } else if (output.length > 128) { clearTimeout(timer); reject(new Error('Unexpected copy guard output')); } });
      guard.once('error', error => { clearTimeout(timer); reject(error); });
      guard.once('exit', () => { clearTimeout(timer); reject(new Error('Copy lease guard stopped')); });
    })));
    let currentStep: CopyStep | undefined;
    const checkpoint = (value: typeof copyCheckpoints[number]) => { if (currentStep) observe({ ...currentStep, checkpoint: value }); };
    const remote: Remote = (side, command, input) => {
      checkpoint('lease_status');
      assert.ok(guards.every(guard => guard.exitCode === null && guard.signalCode === null && guard.connected), 'Copy lease owner stopped');
      checkpoint('pod_read');
      const pod = getPod(pods[side]);
      checkpoint('pod_identity');
      validateCopyPod(pod, side, identities[side].metadata.uid);
      checkpoint('pod_spec');
      assert.deepEqual(pod.spec, identities[side].spec);
      checkpoint('container_identity');
      assert.deepEqual(pod.status.containerStatuses, identities[side].status.containerStatuses);
      const expected = copyIdentities[side];
      checkpoint('exec');
      return execFileSync('kubectl', [
        '--request-timeout=1800s', '-n', expected.namespace, 'exec', ...(input ? ['-i'] : []), pods[side], '-c', 'copy', '--',
        '/bin/sh', '-ec', 'test "$POD_UID" = "$1"; shift; . /policy/capacity.sh; capacity_backup; exec "$@"', 'copy-fenced', pod.metadata.uid, ...command,
      ], { input, timeout: 1_800_000, maxBuffer: copyChunk + 65_536, stdio: ['pipe', 'pipe', 'pipe'] });
    };
    await copyArchive(remote, expectedSha, undefined, true, directory, Number(configuredRate), step => { currentStep = step; observe(step); });
    console.log('Offhost SHA and TOC verified; full isolated restore remains required.');
  } finally {
    for (const guard of guards) guard.kill('SIGTERM');
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  let step: CopyStep | undefined;
  main(current => { step = current; }).catch(reason => {
    console.error(JSON.stringify(copyFailureReceipt(reason, step)));
    console.error('Copy failed; preserve partial and inspect the owned copy Jobs. No automatic retry.');
    process.exitCode = 1;
  });
}
