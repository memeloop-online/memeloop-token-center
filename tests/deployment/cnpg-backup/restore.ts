import assert from 'node:assert/strict';
import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { archiveIdentity } from './volume-identity.ts';
import { restoreSides, validateRestorePlan, validateRestorePod, type RestorePlan } from './restore-guard.ts';

export function verifyRestoreReceipt(log: string, plan: RestorePlan): object {
  validateRestorePlan(plan);
  const candidates = log.split('\n').filter(line => line.startsWith('{"schema":1,"state":"isolated-logical-restore-verified"'));
  assert.equal(candidates.length, 1, 'Expected exactly one final isolated restore receipt');
  const receipt = JSON.parse(candidates[0]!);
  assert.equal(receipt.archive_sha256, plan.archiveSHA256);
  assert.ok(Number.isSafeInteger(receipt.archive_bytes) && receipt.archive_bytes > 0 && receipt.archive_bytes <= 25_769_803_776);
  assert.ok(Number.isSafeInteger(receipt.public_relations) && receipt.public_relations >= 254);
  assert.equal(receipt.backup_fs_uuid, archiveIdentity.filesystemUUID);
  assert.equal(receipt.scratch_fs_uuid, plan.scratch.filesystemUUID);
  for (const name of ['original_ownership_acl_verified', 'application_acceptance_verified', 'physical_wal_protection_verified']) assert.equal(receipt[name], false);
  assert.ok(Number.isFinite(Date.parse(receipt.verified_at)));
  return receipt;
}

async function main(): Promise<void> {
  assert.equal(process.env.PARENT_REVIEW_APPROVED, 'true');
  assert.equal(process.argv.length, 4, 'restore.ts EXISTING_RESTORE_POD REVIEWED_PLAN_JSON');
  const [podName, planFile] = process.argv.slice(2) as [string, string];
  const plan: RestorePlan = JSON.parse(readFileSync(planFile, 'utf8'));
  validateRestorePlan(plan);
  const read = (args: string[]) => execFileSync('kubectl', ['--request-timeout=8s', '-n', archiveIdentity.namespace, ...args], { encoding: 'utf8', timeout: 10_000, killSignal: 'SIGKILL', maxBuffer: 8 * 1024 ** 2, stdio: ['ignore', 'pipe', 'pipe'] });
  const getPod = () => JSON.parse(read(['get', 'pod', podName, '-o', 'json']));
  const initial = getPod();
  validateRestorePod(initial, plan);
  const guards: ChildProcess[] = [];
  try {
    await Promise.all(restoreSides.map(side => new Promise<void>((resolve, reject) => {
      const guard = spawn(process.execPath, [fileURLToPath(new URL('./restore-guard.ts', import.meta.url)), side, podName, initial.metadata.uid, planFile], { stdio: ['ignore', 'pipe', 'pipe', 'ipc'] });
      guards.push(guard);
      guard.stderr!.pipe(process.stderr);
      const timer = setTimeout(() => reject(new Error('Restore lease startup timeout')), 110_000);
      let output = '';
      guard.stdout!.on('data', chunk => {
        output += chunk.toString();
        if (output === 'RESTORE_LEASE_READY\n') { clearTimeout(timer); resolve(); }
        else if (output.length > 128) { clearTimeout(timer); reject(new Error('Unexpected restore guard output')); }
      });
      guard.once('error', error => { clearTimeout(timer); reject(error); });
      guard.once('exit', () => { clearTimeout(timer); reject(new Error('Restore lease guard stopped')); });
    })));
    const started = performance.now();
    while (performance.now() - started < 28_800_000) {
      const pod = getPod();
      assert.deepEqual(pod.spec, initial.spec);
      if (pod.status.phase === 'Succeeded') {
        validateRestorePod(pod, plan, initial.metadata.uid, true);
        assert.equal(pod.status.containerStatuses[0].containerID, initial.status.containerStatuses[0].containerID);
        const log = read(['logs', podName, '-c', 'verify', '--tail=10', '--limit-bytes=8192']);
        const after = getPod();
        validateRestorePod(after, plan, initial.metadata.uid, true);
        assert.deepEqual(after.spec, pod.spec);
        assert.deepEqual(after.status.containerStatuses, pod.status.containerStatuses);
        console.log(JSON.stringify(verifyRestoreReceipt(log, plan)));
        return;
      }
      validateRestorePod(pod, plan, initial.metadata.uid);
      assert.deepEqual(pod.status.containerStatuses, initial.status.containerStatuses);
      assert.ok(guards.every(guard => guard.connected && guard.exitCode === null && guard.signalCode === null), 'Restore guard stopped; no automatic restart');
      await delay(3000);
    }
    throw new Error('Restore controller deadline');
  } finally {
    for (const guard of guards) guard.kill('SIGTERM');
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => {
    console.error('Restore controller stopped; both leases expire independently. Preserve archive and partial scratch; no automatic retry or success claim.');
    process.exitCode = 1;
  });
}
