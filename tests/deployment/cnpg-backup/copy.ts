import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';

export const archiveDirectory = '/backup/mtc-pg-logical-20261004';
export const archiveName = 'memeloop_token_center.dump';
export const archiveLimit = 25_769_803_776;
export const copyChunk = 4 * 1024 * 1024;
export type Remote = (side: 'source' | 'destination', command: string[], input?: Buffer) => Buffer;

export async function copyArchive(remote: Remote, expectedSha: string, pause: (milliseconds: number) => Promise<void> = async milliseconds => { await delay(milliseconds); }): Promise<void> {
  assert.match(expectedSha, /^[0-9a-f]{64}$/);
  const shell = (side: 'source' | 'destination', body: string, input?: Buffer) =>
    remote(side, ['/bin/sh', '-ec', `umask 077; cd ${archiveDirectory}; ${body}`], input);
  const archive = archiveName;
  shell('source', `test -f LOCAL_ARCHIVE_CREATED; printf '%s  ${archive}\n' '${expectedSha}' | sha256sum -c - >/dev/null`);
  const size = Number(shell('source', `stat -c %s ${archive}`).toString().trim());
  assert.ok(Number.isSafeInteger(size) && size > 0 && size <= archiveLimit, 'Source size exceeds bounded archive contract');
  remote('destination', ['/bin/sh', '-ec', `umask 077; mkdir -p ${archiveDirectory}`]);
  const offset = Number(shell('destination', `test ! -e ${archive}; if test -e ${archive}.partial; then stat -c %s ${archive}.partial; else printf 0; fi`).toString().trim());
  assert.ok(Number.isSafeInteger(offset) && offset >= 0 && offset <= size, 'Invalid resume offset');
  for (let position = Math.floor(offset / copyChunk) * copyChunk; position < size; position += copyChunk) {
    const availableKiB = Number(shell('destination', "timeout 5 df -Pk . | awk 'NR == 2 { print $4 }'").toString().trim());
    assert.ok(Number.isFinite(availableKiB) && availableKiB >= 33_558_528, 'Destination must retain 32GiB plus one chunk; preserve partial');
    const chunk = shell('source', `dd if=${archive} bs=4194304 skip=${position / copyChunk} count=1 iflag=fullblock status=none`);
    assert.equal(chunk.length, Math.min(copyChunk, size - position), 'Short source read');
    shell('destination', `prlimit --core=0:0 --fsize=25769803776:25769803776 -- dd of=${archive}.partial bs=4194304 seek=${position / copyChunk} count=1 iflag=fullblock conv=notrunc,fsync status=none`, chunk);
    await pause(chunk.length / 1024 / 1024 * 1000);
  }
  shell('destination', `test "$(stat -c %s ${archive}.partial)" -eq ${size}; printf '%s  ${archive}.partial\n' '${expectedSha}' | sha256sum -c - >/dev/null; prlimit --core=0:0 --fsize=16777216:16777216 -- pg_restore --list ${archive}.partial > ${archive}.list.partial; test -s ${archive}.list.partial; mv ${archive}.partial ${archive}; mv ${archive}.list.partial ${archive}.list; printf '%s  ${archive}\n' '${expectedSha}' > ${archive}.sha256; sync -f .; touch LOCAL_ARCHIVE_CREATED OFFHOST_COPY_VERIFIED; sync -f .`);
}

function validatePod(pod: Record<string, any>, source: boolean): void {
  assert.equal(pod.metadata.namespace, 'memeloop-token-center');
  assert.equal(pod.metadata.labels['recovery.mtc/operation'], 'pg-logical-backup-20261004');
  assert.equal(pod.metadata.labels['recovery.mtc/role'], 'copy');
  assert.equal(pod.spec.nodeName, source ? 'haixia' : 'versetensor-hv');
  assert.equal(pod.status.phase, 'Running');
  const claim = pod.spec.volumes.find((volume: Record<string, any>) => volume.name === 'backup');
  assert.equal(claim.persistentVolumeClaim.claimName, source ? 'mtc-pg-local-stage-20261004' : 'mtc-pg-logical-backup-20261003');
  assert.equal(pod.spec.containers[0].volumeMounts.find((mount: Record<string, any>) => mount.name === 'backup').readOnly, source);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  assert.equal(process.env.PARENT_REVIEW_APPROVED, 'true', 'Parent review required; this command never starts a dump');
  const [sourcePod, destinationPod, expectedSha] = process.argv.slice(2);
  assert.ok(sourcePod && destinationPod && expectedSha, 'Usage: copy.ts SOURCE_COPY_POD DESTINATION_COPY_POD SOURCE_SHA256');
  const pods = { source: sourcePod, destination: destinationPod };
  for (const side of ['source', 'destination'] as const) {
    assert.match(pods[side], /^mtc-pg-copy-(source|destination)-20261004-[a-z0-9-]+$/);
    const pod = JSON.parse(execFileSync('kubectl', ['--request-timeout=15s', '-n', 'memeloop-token-center', 'get', 'pod', pods[side], '-o', 'json'], { encoding: 'utf8', timeout: 20_000 }));
    validatePod(pod, side === 'source');
  }
  const remote: Remote = (side, command, input) => execFileSync('kubectl', [
    '--request-timeout=1800s', '-n', 'memeloop-token-center', 'exec', ...(input ? ['-i'] : []), pods[side], '-c', 'copy', '--', ...command,
  ], { input, timeout: 1_800_000, maxBuffer: copyChunk + 65_536 });
  await copyArchive(remote, expectedSha);
  console.log('Offhost SHA and TOC verified; full isolated restore remains required.');
}
