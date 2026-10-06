import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { copyIdentities, copySides, validateCopyPod } from './copy-guard.ts';

export const archiveDirectory = '/backup/mtc-pg-logical-20261004';
export const archiveName = 'memeloop_token_center.dump';
export const archiveLimit = 25_769_803_776;
export const copyChunk = 4 * 1024 * 1024;
export type Remote = (side: 'source' | 'destination', command: string[], input?: Buffer) => Buffer;

export async function copyArchive(remote: Remote, expectedSha: string, pause: (milliseconds: number) => Promise<void> = async milliseconds => { await delay(milliseconds); }, hardCapacity = false, directory = archiveDirectory): Promise<void> {
  assert.match(expectedSha, /^[0-9a-f]{64}$/);
  assert.match(directory, /^\/backup\/mtc-pg-logical-[0-9]{8}(?:-[a-z0-9]+)*$/);
  const capacityCheck = (side: 'source' | 'destination') => {
    if (hardCapacity) remote(side, ['/bin/sh', '-ec', '. /policy/capacity.sh; capacity_backup']);
  };
  capacityCheck('source');
  capacityCheck('destination');
  const shell = (side: 'source' | 'destination', body: string, input?: Buffer) =>
    remote(side, ['/bin/sh', '-ec', `umask 077; cd ${directory}; ${body}`], input);
  const archive = archiveName;
  shell('source', `test -f LOCAL_ARCHIVE_CREATED; printf '%s  ${archive}\n' '${expectedSha}' | sha256sum -c - >/dev/null`);
  const size = Number(shell('source', `stat -c %s ${archive}`).toString().trim());
  assert.ok(Number.isSafeInteger(size) && size > 0 && size <= archiveLimit, 'Source size exceeds bounded archive contract');
  remote('destination', ['/bin/sh', '-ec', `umask 077; mkdir -p ${directory}`]);
  if (hardCapacity) shell('destination', 'test ! -e LOCAL_ARCHIVE_CREATED; test ! -e OFFHOST_COPY_VERIFIED');
  const offset = Number(shell('destination', `test ! -e ${archive}; if test -e ${archive}.partial; then stat -c %s ${archive}.partial; else printf 0; fi`).toString().trim());
  assert.ok(Number.isSafeInteger(offset) && offset >= 0 && offset <= size, 'Invalid resume offset');
  for (let position = Math.floor(offset / copyChunk) * copyChunk; position < size; position += copyChunk) {
    capacityCheck('source');
    capacityCheck('destination');
    const availableKiB = Number(shell('destination', "timeout 5 df -Pk . | awk 'NR == 2 { print $4 }'").toString().trim());
    const reserveKiB = hardCapacity ? 266_240 : 33_558_528;
    assert.ok(Number.isFinite(availableKiB) && availableKiB >= reserveKiB, 'Destination must retain its reviewed reserve plus one chunk; preserve partial');
    const chunk = shell('source', `dd if=${archive} bs=4194304 skip=${position / copyChunk} count=1 iflag=fullblock status=none`);
    assert.equal(chunk.length, Math.min(copyChunk, size - position), 'Short source read');
    shell('destination', `prlimit --core=0:0 --fsize=25769803776:25769803776 -- dd of=${archive}.partial bs=4194304 seek=${position / copyChunk} count=1 iflag=fullblock conv=notrunc,fsync status=none`, chunk);
    await pause(chunk.length / 1024 / 1024 * 1000);
  }
  capacityCheck('destination');
  const markerTrap = hardCapacity ? `trap 'marker_status=$?; if test "$marker_status" -ne 0; then rm -f LOCAL_ARCHIVE_CREATED OFFHOST_COPY_VERIFIED; fi; exit "$marker_status"' EXIT; ` : '';
  const receiptGuard = hardCapacity ? '. /policy/capacity.sh; capacity_backup; ' : '';
  shell('destination', `${markerTrap}test "$(stat -c %s ${archive}.partial)" -eq ${size}; printf '%s  ${archive}.partial\n' '${expectedSha}' | sha256sum -c - >/dev/null; prlimit --core=0:0 --fsize=16777216:16777216 -- pg_restore --list ${archive}.partial > ${archive}.list.partial; test -s ${archive}.list.partial; ${receiptGuard}mv ${archive}.partial ${archive}; mv ${archive}.list.partial ${archive}.list; printf '%s  ${archive}\n' '${expectedSha}' > ${archive}.sha256; sync -f .; ${receiptGuard}touch LOCAL_ARCHIVE_CREATED OFFHOST_COPY_VERIFIED; sync -f .; ${hardCapacity ? 'capacity_backup' : ':'}`);
}

async function main(): Promise<void> {
  assert.equal(process.env.PARENT_REVIEW_APPROVED, 'true', 'Parent review required; this command never starts a dump');
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
  const guard = spawn(process.execPath, [fileURLToPath(new URL('./copy-guard.ts', import.meta.url)), ...copySides.flatMap(side => [pods[side], identities[side].metadata.uid])], { stdio: ['ignore', 'pipe', 'pipe'] });
  guard.stderr.pipe(process.stderr);
  try {
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('Copy lease startup timeout')), 110_000);
      let output = '';
      guard.stdout.on('data', chunk => { output += chunk.toString(); if (output === 'COPY_LEASES_READY\n') { clearTimeout(timer); resolve(); } else if (output.length > 128) { clearTimeout(timer); reject(new Error('Unexpected copy guard output')); } });
      guard.once('error', error => { clearTimeout(timer); reject(error); });
      guard.once('exit', () => { clearTimeout(timer); reject(new Error('Copy lease guard stopped')); });
    });
    const remote: Remote = (side, command, input) => {
      const pod = getPod(pods[side]);
      validateCopyPod(pod, side, identities[side].metadata.uid);
      assert.deepEqual(pod.spec, identities[side].spec);
      assert.deepEqual(pod.status.containerStatuses, identities[side].status.containerStatuses);
      const expected = copyIdentities[side];
      return execFileSync('kubectl', [
        '--request-timeout=1800s', '-n', expected.namespace, 'exec', ...(input ? ['-i'] : []), pods[side], '-c', 'copy', '--',
        '/bin/sh', '-ec', 'test "$POD_UID" = "$1"; shift; . /policy/capacity.sh; capacity_backup; exec "$@"', 'copy-fenced', pod.metadata.uid, ...command,
      ], { input, timeout: 1_800_000, maxBuffer: copyChunk + 65_536, stdio: ['pipe', 'pipe', 'pipe'] });
    };
    await copyArchive(remote, expectedSha, undefined, true, directory);
    console.log('Offhost SHA and TOC verified; full isolated restore remains required.');
  } finally {
    guard.kill('SIGTERM');
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => {
    console.error('Copy failed; preserve partial and inspect the owned copy Jobs. No automatic retry.');
    process.exitCode = 1;
  });
}
