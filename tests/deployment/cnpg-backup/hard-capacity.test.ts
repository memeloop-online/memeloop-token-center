import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { stringify } from 'yaml';
import { capacityPolicy } from './capacity-policy.ts';
import { archiveDirectory, archiveName, copyArchive, type Remote } from './copy.ts';
import { boundedClaims, boundedJobs, preparedResources } from './hard-capacity.ts';
import { storagePlan, validateStorageResources } from './storage-preflight.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const resources = preparedResources();
const jobs = resources.filter(resource => resource.kind === 'Job');
const stage = jobs.find(resource => resource.metadata.name === boundedJobs.stage).spec.template.spec.containers[0];
const restore = jobs.find(resource => resource.metadata.name === boundedJobs.restore).spec.template.spec.containers[0];
const image = 'ghcr.io/cloudnative-pg/postgresql:17.5';
const containers: string[] = [];
const mib = 1024 ** 2;
const docker = (args: string[], input?: Buffer | string) => execFileSync('docker', args, { input, timeout: 180_000, maxBuffer: 20 * mib });
const execute = (container: string, command: string[], input?: Buffer | string) => docker(['exec', ...(input === undefined ? [] : ['-i']), container, ...command], input);
const shell = (container: string, script: string) => execute(container, ['/bin/sh', '-ec', script]);
const write = (container: string, path: string, data: string | Buffer) => execute(container, ['dd', `of=${path}`, 'status=none'], data);

test('prepared resources cannot provision unbounded storage or start a production operation', () => {
  const claims = resources.filter(resource => resource.kind === 'PersistentVolumeClaim');
  assert.equal(claims.length, 3);
  for (const claim of claims) {
    assert.equal(claim.spec.volumeName, claim.metadata.name);
    assert.equal(claim.spec.storageClassName, storagePlan.volumes.find((volume: any) => volume.name === claim.metadata.name).storageClass);
    assert.equal(claim.spec.volumeMode, 'Filesystem');
    assert.equal(claim.spec.resources.requests.storage, claim.metadata.name === boundedClaims.scratch ? '64Gi' : '28Gi');
  }
  for (const job of jobs) {
    assert.equal(job.spec.suspend, true);
    assert.equal(job.spec.backoffLimit, 0);
    const pod = job.spec.template.spec;
    assert.equal(pod.hostPID, undefined);
    assert.equal(pod.hostNetwork, undefined);
    assert.equal(pod.automountServiceAccountToken, false);
    assert.equal(pod.securityContext.runAsUser, 26);
    assert.ok(pod.volumes.every((volume: any) => !volume.hostPath));
    const container = pod.containers[0];
    assert.equal(container.securityContext.allowPrivilegeEscalation, false);
    assert.deepEqual(container.securityContext.capabilities.drop, ['ALL']);
    assert.equal(container.env.find((entry: any) => entry.name === 'HARD_CAPACITY_REVIEW_APPROVED').value, 'false');
    assert.equal(container.env.find((entry: any) => entry.name === 'EXPECTED_BACKUP_FS_UUID').value, '');
    assert.match(job.metadata.annotations['recovery.mtc/long-term-protection'], /remain-open/);
    for (const volume of pod.volumes.filter((entry: any) => entry.persistentVolumeClaim)) {
      assert.ok(Object.values(boundedClaims).includes(volume.persistentVolumeClaim.claimName));
    }
  }
  assert.equal(stage.env.find((entry: any) => entry.name === 'EXPECTED_SERVER_ADDRESS').value, '');
  assert.equal(stage.env.find((entry: any) => entry.name === 'SOURCE_SPACE_LEASE_SECONDS').value, '45');
  assert.equal(stage.env.find((entry: any) => entry.name === 'BACKUP_UUID_ATTESTATION').value, 'external-csi-lease');
  assert.equal(stage.env.find((entry: any) => entry.name === 'POD_UID').valueFrom.fieldRef.fieldPath, 'metadata.uid');
  assert.ok(stage.command[6].indexOf('source_space_wait') < stage.command[6].indexOf('capacity_backup'));
  assert.doesNotMatch(stage.command[6], /actual_backup_fs_uuid=/);
  assert.match(stage.env.find((entry: any) => entry.name === 'PGOPTIONS').value, /temp_file_limit=0/);
  const stagePod = jobs.find(job => job.metadata.name === boundedJobs.stage).spec.template.spec;
  assert.deepEqual(stagePod.volumes.filter((volume: any) => volume.persistentVolumeClaim).map((volume: any) => volume.persistentVolumeClaim.claimName), [boundedClaims.stage]);
  assert.deepEqual(stagePod.volumes.find((volume: any) => volume.name === 'tmp').emptyDir, { medium: 'Memory', sizeLimit: '64Mi' });
  for (const required of ['sleep 0.0625', 'wait "$dump_pid"', 'wait "$rate_pid"', '--fsize=25769803776:25769803776', 'capacity_backup']) {
    assert.ok(stage.command[6].includes(required));
  }
  assert.ok(!stage.command[6].includes('67108864'));
  assert.ok(!stage.command[6].includes('35651584'));
  assert.equal(jobs.find(job => job.metadata.name === boundedJobs.stage).spec.parallelism, 1);
  const restorePod = jobs.find(job => job.metadata.name === boundedJobs.restore).spec.template.spec;
  assert.equal(restorePod.volumes.find((volume: any) => volume.name === 'backup').persistentVolumeClaim.readOnly, true);
  assert.ok(!restore.env.some((entry: any) => entry.valueFrom));
  const isolation = resources.find(resource => resource.kind === 'NetworkPolicy' && resource.metadata.name === 'mtc-pg-restore-isolation-20261004');
  assert.deepEqual(isolation.spec.policyTypes, ['Ingress', 'Egress']);
  assert.equal(isolation.spec.ingress, undefined);
  assert.equal(isolation.spec.egress, undefined);
  const manifest = resources.map(resource => stringify(resource)).join('---\n');
  assert.doesNotMatch(manifest, /\b(?:mkfs|losetup|fallocate)\b|storageClassName: local-path/);
  validateStorageResources(resources.filter(resource => ['StorageClass', 'Volume', 'PersistentVolume', 'PersistentVolumeClaim'].includes(resource.kind)));
  const coreManifest = resources.filter(resource => resource.apiVersion !== 'longhorn.io/v1beta2').map(resource => stringify(resource)).join('---\n');
  execFileSync('/tmp/kubeconform', ['-strict', '-summary', '-exit-on-error', '-'], { input: coreManifest, timeout: 90_000 });
});

function fixture(backupMiB = 64, scratchMiB = 256, backupInodes = 8192): string {
  const environment: Record<string, string> = {
    PARENT_REVIEW_APPROVED: 'true', SOURCE_IO_REVIEW_APPROVED: 'true', ROOT_STORAGE_REVIEW_APPROVED: 'true',
    HARD_CAPACITY_REVIEW_APPROVED: 'true', EXPECTED_BACKUP_FS_UUID: 'fixture-backup', EXPECTED_SCRATCH_FS_UUID: 'fixture-scratch',
    BACKUP_MAX_BYTES: String(backupMiB * mib), BACKUP_MIN_BYTES: String(mib), BACKUP_RESERVE_BYTES: '0',
    SCRATCH_MAX_BYTES: String(scratchMiB * mib), SCRATCH_MIN_BYTES: String(mib), SCRATCH_RESERVE_BYTES: '0',
    CAPACITY_MIN_FREE_INODES: '1', EXPECTED_SERVER_ADDRESS: '127.0.0.1',
    DATABASE_URL: 'host=/scratch/socket user=postgres dbname=source',
    PGOPTIONS: '', HOME: '/tmp',
    SOURCE_SPACE_LEASE_SECONDS: '45', SOURCE_SPACE_WAIT_SECONDS: '3',
  };
  const container = docker(['run', '-d', '--network=none', '--read-only', '--user=26:26', '--cap-drop=ALL', '--security-opt=no-new-privileges',
    ...Object.entries(environment).flatMap(([name, value]) => ['--env', `${name}=${value}`]),
    '--tmpfs', '/tmp:rw,exec,size=32m,uid=26,gid=26', '--tmpfs', '/policy:rw,size=1m,uid=26,gid=26',
    '--tmpfs', `/backup:rw,size=${backupMiB}m,nr_inodes=${backupInodes},uid=26,gid=26`,
    '--tmpfs', `/scratch:rw,size=${scratchMiB}m,nr_inodes=8192,uid=26,gid=26`, '--entrypoint=/bin/sleep', image, '1200']).toString().trim();
  containers.push(container);
  shell(container, 'mkdir /tmp/bin');
  write(container, '/policy/capacity.sh', capacityPolicy);
  write(container, '/tmp/bin/findmnt', String.raw`#!/bin/sh
set -eu
test ! -e /tmp/findmnt-fail
if test -e /tmp/findmnt-slow; then sleep 20; fi
case "$*" in
  *'/tmp') printf tmpfs; exit ;;
  *'/scratch') fixture_path=/scratch; fixture_uuid=fixture-scratch ;;
  *) fixture_path=/backup; fixture_uuid=fixture-backup ;;
esac
case "$*" in
  *FSTYPE*) if test -e /tmp/wrong-type; then printf overlay; else printf ext4; fi ;;
  *FSROOT*) if test -e /tmp/subdirectory; then printf /unbounded-local-path; else printf /; fi ;;
  *TARGET*) printf %s "$fixture_path" ;;
  *UUID*) if test ! -e /tmp/empty-uuid; then printf %s "$fixture_uuid"; fi ;;
  *MAJ:MIN*) printf 8:32 ;;
  *SOURCE*) printf /dev/fixture ;;
  *) exit 1 ;;
esac
`);
  shell(container, 'chmod 700 /tmp/bin/findmnt');
  return container;
}

function run(container: string, command: string[], environment: Record<string, string> = {}): Buffer {
  const path = shell(container, 'printf %s "$PATH"').toString();
  return execute(container, ['env', `PATH=/tmp/bin:${path}`, ...Object.entries(environment).map(([name, value]) => `${name}=${value}`), ...command]);
}

function check(container: string, environment: Record<string, string> = {}): Buffer {
  return run(container, ['/bin/sh', '-ec', '. /policy/capacity.sh; capacity_backup'], environment);
}

function initializeSource(container: string): void {
  const actualPsql = shell(container, 'command -v psql').toString().trim();
  shell(container, 'mkdir /scratch/socket; initdb -D /scratch/source -U postgres --auth-local=trust --auth-host=reject >/tmp/init.log; pg_ctl -D /scratch/source -l /tmp/postgres.log -o "-c listen_addresses= -c unix_socket_directories=/scratch/socket" -w start');
  execute(container, ['createdb', '-h', '/scratch/socket', '-U', 'postgres', 'source']);
  execute(container, [actualPsql, '-h', '/scratch/socket', '-U', 'postgres', '-d', 'source', '-v', 'ON_ERROR_STOP=1', '-c', "DO $$ BEGIN FOR counter IN 1..253 LOOP EXECUTE format('CREATE TABLE public.fixture_%s (id integer)', counter); END LOOP; END $$; CREATE TABLE public.payload AS SELECT entry AS id, repeat(md5(entry::text), 32) AS body FROM generate_series(1,8192) entry;"]);
  write(container, '/tmp/bin/psql', '#!/bin/sh\ncase "$*" in *"SELECT NOT pg_is_in_recovery()"*) printf "t\\n" ;; *) exec ' + actualPsql + ' "$@" ;; esac\n');
  shell(container, 'chmod 700 /tmp/bin/psql');
}

function stageCommand(): string[] {
  const command = [...stage.command];
  command[6] = command[6].replace('26071793664', '1048576');
  return command;
}

test('kernel-enforced block/inode limits and isolated restore receipts fail closed', { timeout: 600_000 }, async context => {
  context.after(() => { for (const container of containers) docker(['rm', '-f', container]); });
  docker(['pull', image]);
  await context.test('mount identity, real filesystem capacity, inode and block watermarks are mandatory', () => {
    const candidate = fixture();
    check(candidate);
    const denied: Record<string, string>[] = [
      { HARD_CAPACITY_REVIEW_APPROVED: 'false' }, { EXPECTED_BACKUP_FS_UUID: '' }, { EXPECTED_BACKUP_FS_UUID: 'wrong' },
      { BACKUP_MAX_BYTES: String(32 * mib) }, { BACKUP_MIN_BYTES: String(65 * mib) },
      { BACKUP_RESERVE_BYTES: String(65 * mib) }, { CAPACITY_MIN_FREE_INODES: '8193' },
    ];
    for (const environment of denied) assert.throws(() => check(candidate, environment));
    for (const marker of ['wrong-type', 'subdirectory', 'findmnt-fail', 'findmnt-slow']) {
      shell(candidate, `touch /tmp/${marker}`);
      assert.throws(() => check(candidate));
      shell(candidate, `rm /tmp/${marker}`);
    }
    assert.throws(() => run(candidate, ['/bin/sh', '-ec', '. /policy/capacity.sh; capacity_restore'], { EXPECTED_SCRATCH_FS_UUID: 'fixture-backup' }));
  });
  await context.test('many individually small files cannot exceed the aggregate kernel block cap', () => {
    const candidate = fixture(4);
    assert.throws(() => shell(candidate, 'for index in 1 2 3 4 5; do dd if=/dev/zero of=/backup/part-$index bs=1048576 count=1 status=none; done'));
    assert.ok(Number(shell(candidate, "du -sk /backup | awk '{print $1}'").toString()) <= 4096);
    assert.throws(() => check(candidate, { BACKUP_RESERVE_BYTES: '4096' }));
    shell(candidate, 'test ! -e /scratch/RESTORE_SUCCESS.json');
  });
  await context.test('rootless empty UUID requires a fresh pod-bound CSI identity and the same mounted device', () => {
    const candidate = fixture();
    shell(candidate, 'touch /tmp/empty-uuid');
    const environment = { BACKUP_UUID_ATTESTATION: 'external-csi-lease', EXPECTED_BACKUP_DEVICE: '/dev/fixture', POD_UID: 'fixture-pod' };
    assert.throws(() => check(candidate));
    assert.throws(() => check(candidate, environment));
    const epoch = Math.floor(Date.now() / 1000);
    const lease = `${epoch} fixture-pod /dev/fixture fixture-backup 8:32`;
    write(candidate, '/tmp/backup-volume.lease', lease);
    check(candidate, environment);
    for (const invalid of [
      lease.replace(String(epoch), '1'), lease.replace(String(epoch), '9999999999'),
      lease.replace('fixture-pod', 'replaced-pod'), lease.replace('/dev/fixture', '/dev/other'),
      lease.replace('fixture-backup', 'wrong-uuid'), lease.replace('8:32', '8:33'),
      lease + ' extra', lease.replace(String(epoch), 'invalid'),
    ]) {
      write(candidate, '/tmp/backup-volume.lease', invalid);
      assert.throws(() => check(candidate, environment));
    }
    write(candidate, '/tmp/backup-volume.lease', lease);
    assert.throws(() => check(candidate, { ...environment, EXPECTED_BACKUP_DEVICE: '/dev/other' }));
    shell(candidate, 'rm /tmp/empty-uuid');
    write(candidate, '/tmp/backup-volume.lease', lease.replace('fixture-backup', 'wrong-uuid'));
    assert.throws(() => check(candidate, { ...environment, EXPECTED_BACKUP_FS_UUID: 'wrong-uuid' }));
  });
  await context.test('inode exhaustion also fails closed without a large file', () => {
    const candidate = fixture(4, 256, 128);
    assert.throws(() => shell(candidate, 'index=0; while test "$index" -lt 256; do touch "/backup/inode-$index"; index=$((index + 1)); done'));
    assert.throws(() => check(candidate));
    shell(candidate, 'test ! -e /scratch/RESTORE_SUCCESS.json');
  });

  const source = fixture();
  initializeSource(source);
  shell(source, 'date +%s > /tmp/source-space.lease');
  run(source, stageCommand(), { PGOPTIONS: '-c default_transaction_read_only=on -c lock_timeout=5s -c temp_file_limit=0' });
  const archive = shell(source, `cat ${archiveDirectory}/${archiveName}`);
  const expectedSha = createHash('sha256').update(archive).digest('hex');
  const seed = (candidate: string, offhost = true) => {
    shell(candidate, `mkdir ${archiveDirectory}`);
    write(candidate, `${archiveDirectory}/${archiveName}`, archive);
    write(candidate, `${archiveDirectory}/${archiveName}.sha256`, `${expectedSha}  ${archiveName}\n`);
    shell(candidate, `touch ${archiveDirectory}/LOCAL_ARCHIVE_CREATED${offhost ? ` ${archiveDirectory}/OFFHOST_COPY_VERIFIED` : ''}`);
  };
  await context.test('export ENOSPC retains partial without local completion or restore success', () => {
    const candidate = fixture(4);
    initializeSource(candidate);
    shell(candidate, 'date +%s > /tmp/source-space.lease');
    assert.throws(() => run(candidate, stageCommand()));
    shell(candidate, `test -e ${archiveDirectory}/${archiveName}.partial; test ! -e ${archiveDirectory}/LOCAL_ARCHIVE_CREATED; test ! -e /scratch/RESTORE_SUCCESS.json`);
  });
  await context.test('missing, stale and future source-space leases fail closed', () => {
    const candidate = fixture();
    const lease = () => shell(candidate, '. /policy/capacity.sh; source_space_lease');
    assert.throws(lease);
    for (const value of ['bad', '1', '9999999999']) {
      write(candidate, '/tmp/source-space.lease', value);
      assert.throws(lease);
    }
    shell(candidate, 'date +%s > /tmp/source-space.lease');
    lease();
    shell(candidate, 'rm /tmp/source-space.lease');
    assert.throws(() => run(candidate, stageCommand()));
    shell(candidate, `test ! -e ${archiveDirectory}/LOCAL_ARCHIVE_CREATED`);
  });
  await context.test('revoked source-space lease aborts active dump without touching the source server', async () => {
    const candidate = fixture();
    initializeSource(candidate);
    shell(candidate, 'date +%s > /tmp/source-space.lease');
    const path = shell(candidate, 'printf %s "$PATH"').toString();
    const child = spawn('docker', ['exec', '--env', `PATH=/tmp/bin:${path}`, candidate, ...stageCommand()], { stdio: 'ignore' });
    const completed = new Promise<number | null>((resolve, reject) => { child.on('error', reject); child.on('exit', resolve); });
    try {
      let active = false;
      for (let attempt = 0; attempt < 50 && child.exitCode === null; attempt++) {
        if (shell(candidate, `if test -s ${archiveDirectory}/${archiveName}.partial; then printf active; fi`).toString() === 'active') { active = true; break; }
        await delay(100);
      }
      assert.ok(active, 'Fixture must revoke during active export, not just preflight');
      shell(candidate, 'rm /tmp/source-space.lease');
      assert.notEqual(await completed, 0);
      shell(candidate, `test -s ${archiveDirectory}/${archiveName}.partial; test ! -e ${archiveDirectory}/LOCAL_ARCHIVE_CREATED; pg_ctl -D /scratch/source status`);
    } finally {
      if (child.exitCode === null) child.kill();
    }
  });
  await context.test('revoked CSI identity stops an active rootless export without source writes or success markers', async () => {
    const candidate = fixture();
    initializeSource(candidate);
    shell(candidate, 'touch /tmp/empty-uuid; date +%s > /tmp/source-space.lease');
    write(candidate, '/tmp/backup-volume.lease', `${Math.floor(Date.now() / 1000)} fixture-pod /dev/fixture fixture-backup 8:32`);
    const path = shell(candidate, 'printf %s "$PATH"').toString();
    const child = spawn('docker', ['exec', '--env', `PATH=/tmp/bin:${path}`, '--env', 'BACKUP_UUID_ATTESTATION=external-csi-lease', '--env', 'EXPECTED_BACKUP_DEVICE=/dev/fixture', '--env', 'POD_UID=fixture-pod', candidate, ...stageCommand()], { stdio: 'ignore' });
    const completed = new Promise<number | null>((resolve, reject) => { child.on('error', reject); child.on('exit', resolve); });
    try {
      let active = false;
      for (let attempt = 0; attempt < 50 && child.exitCode === null; attempt++) {
        if (shell(candidate, `if test -s ${archiveDirectory}/${archiveName}.partial; then printf active; fi`).toString() === 'active') { active = true; break; }
        await delay(100);
      }
      assert.ok(active, 'Valid external identity must allow an actual rootless export to start');
      shell(candidate, 'rm /tmp/backup-volume.lease');
      assert.notEqual(await completed, 0);
      shell(candidate, `test -s ${archiveDirectory}/${archiveName}.partial; test ! -e ${archiveDirectory}/LOCAL_ARCHIVE_CREATED; pg_ctl -D /scratch/source status`);
    } finally {
      if (child.exitCode === null) child.kill();
    }
  });
  await context.test('copy verifies hard capacity before writing or promoting a partial', async () => {
    const candidate = fixture(4);
    const remote: Remote = (side, command, input) => {
      const target = side === 'source' ? source : candidate;
      const path = shell(target, 'printf %s "$PATH"').toString();
      return execute(target, ['env', `PATH=/tmp/bin:${path}`, ...command], input);
    };
    await assert.rejects(copyArchive(remote, expectedSha, async () => {}, true), /reviewed reserve/);
    shell(candidate, `test ! -e ${archiveDirectory}/OFFHOST_COPY_VERIFIED; test ! -e /scratch/RESTORE_SUCCESS.json`);
  });
  for (const mode of ['copy-enospc', 'copy-marker-sync-failure'] as const) {
    await context.test(`${mode} cannot acquire copy completion markers`, async () => {
      const candidate = fixture(512);
      let filled = false;
      if (mode === 'copy-marker-sync-failure') {
        const actual = shell(candidate, 'command -v sync').toString().trim();
        write(candidate, '/tmp/bin/sync', `#!/bin/sh\ntest ! -e ${archiveDirectory}/OFFHOST_COPY_VERIFIED || exit 42\nexec ${actual} "$@"\n`);
        shell(candidate, 'chmod 700 /tmp/bin/sync');
      }
      const remote: Remote = (side, command, input) => {
        const target = side === 'source' ? source : candidate;
        if (mode === 'copy-enospc' && side === 'destination' && input && !filled) {
          shell(candidate, 'dd if=/dev/zero of=/backup/concurrent-writer bs=1048576 count=509 status=none');
          filled = true;
        }
        const path = shell(target, 'printf %s "$PATH"').toString();
        return execute(target, ['env', `PATH=/tmp/bin:${path}`, ...command], input);
      };
      await assert.rejects(copyArchive(remote, expectedSha, async () => {}, true));
      shell(candidate, `test ! -e ${archiveDirectory}/OFFHOST_COPY_VERIFIED; test ! -e ${archiveDirectory}/LOCAL_ARCHIVE_CREATED; test ! -e /scratch/RESTORE_SUCCESS.json`);
      if (mode === 'copy-enospc') shell(candidate, `test -e ${archiveDirectory}/${archiveName}.partial; rm /backup/concurrent-writer`);
    });
  }
  for (const mode of ['missing-copy-marker', 'wrong-digest', 'corrupt-archive', 'restore-enospc', 'shutdown-failure', 'receipt-sync-failure'] as const) {
    await context.test(`${mode} cannot produce a restore success receipt`, () => {
      const candidate = fixture(64, mode === 'restore-enospc' ? 48 : 256);
      seed(candidate, mode !== 'missing-copy-marker');
      if (mode === 'corrupt-archive') shell(candidate, `printf X | dd of=${archiveDirectory}/${archiveName} conv=notrunc status=none`);
      if (mode === 'shutdown-failure') {
        const actual = shell(candidate, 'command -v pg_ctl').toString().trim();
        write(candidate, '/tmp/bin/pg_ctl', `#!/bin/sh\ncase "$*" in *"-m fast"*) exit 42 ;; *) exec ${actual} "$@" ;; esac\n`);
        shell(candidate, 'chmod 700 /tmp/bin/pg_ctl');
      }
      if (mode === 'receipt-sync-failure') {
        const actual = shell(candidate, 'command -v sync').toString().trim();
        write(candidate, '/tmp/bin/sync', `#!/bin/sh\ntest ! -e /scratch/RESTORE_SUCCESS.json || exit 42\nexec ${actual} "$@"\n`);
        shell(candidate, 'chmod 700 /tmp/bin/sync');
      }
      assert.throws(() => run(candidate, restore.command, { PGOPTIONS: '', EXPECTED_SOURCE_SHA256: mode === 'wrong-digest' ? '0'.repeat(64) : expectedSha }));
      shell(candidate, 'test ! -e /scratch/RESTORE_SUCCESS.json');
    });
  }
  await context.test('only a complete offhost copy, isolated restore and clean stop earn a digest-bound receipt', async () => {
    const candidate = fixture(512);
    const remote: Remote = (side, command, input) => {
      const target = side === 'source' ? source : candidate;
      const path = shell(target, 'printf %s "$PATH"').toString();
      return execute(target, ['env', `PATH=/tmp/bin:${path}`, ...command], input);
    };
    await copyArchive(remote, expectedSha, async () => {}, true);
    shell(candidate, `test -e ${archiveDirectory}/OFFHOST_COPY_VERIFIED; test ! -e /scratch/RESTORE_SUCCESS.json`);
    const output = run(candidate, restore.command, { PGOPTIONS: '', EXPECTED_SOURCE_SHA256: expectedSha });
    assert.match(output.toString(), /full archive restore completed/);
    const receipt = JSON.parse(shell(candidate, 'cat /scratch/RESTORE_SUCCESS.json').toString());
    assert.equal(receipt.schema, 1);
    assert.equal(receipt.state, 'isolated-logical-restore-verified');
    assert.equal(receipt.archive_sha256, expectedSha);
    assert.equal(receipt.archive_bytes, archive.length);
    assert.equal(receipt.public_relations, 254);
    assert.equal(receipt.physical_wal_protection_verified, false);
    assert.equal(receipt.original_ownership_acl_verified, false);
    assert.equal(receipt.application_acceptance_verified, false);
    assert.throws(() => shell(candidate, 'pg_ctl -D /scratch/pgdata status'));
    assert.throws(() => run(candidate, restore.command, { PGOPTIONS: '', EXPECTED_SOURCE_SHA256: expectedSha }));
    assert.deepEqual(JSON.parse(shell(candidate, 'cat /scratch/RESTORE_SUCCESS.json').toString()), receipt);
    console.log(`Bounded restore receipt verified: sha=${expectedSha}, bytes=${archive.length}, public_relations=254; physical/WAL protection remains open`);
  });
});
