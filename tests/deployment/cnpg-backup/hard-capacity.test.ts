import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { stringify } from 'yaml';
import { capacityPolicy } from './capacity-policy.ts';
import { copyContainerCommand } from './copy-guard.ts';
import { archiveDirectory, archiveName, copyArchive, type Remote } from './copy.ts';
import { boundedClaims, boundedJobs, preparedResources } from './hard-capacity.ts';
import { storagePlan, validateStorageResources } from './storage-preflight.ts';
import { exportRateSelection } from './export-rate.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const resources = preparedResources();
const jobs = resources.filter(resource => resource.kind === 'Job');
const stage = jobs.find(resource => resource.metadata.name === boundedJobs.stage).spec.template.spec.containers[0];
const exportOptions = stage.env.find((entry: any) => entry.name === 'PGOPTIONS').value as string;
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
  assert.equal(stage.env.find((entry: any) => entry.name === 'BACKUP_RATE_MIB_PER_SECOND').value, '1');
  assert.ok(stage.command[6].includes(exportRateSelection));
  assert.ok(stage.command[6].indexOf(exportRateSelection) < stage.command[6].indexOf('source_space_wait'));
  assert.ok(stage.command[6].includes('dd bs="$backup_rate_chunk_bytes" count=1 iflag=fullblock'));
  assert.equal(stage.env.find((entry: any) => entry.name === 'SOURCE_SPACE_LEASE_SECONDS').value, '45');
  assert.equal(stage.env.find((entry: any) => entry.name === 'BACKUP_UUID_ATTESTATION').value, 'external-csi-lease');
  assert.equal(stage.env.find((entry: any) => entry.name === 'POD_UID').valueFrom.fieldRef.fieldPath, 'metadata.uid');
  assert.ok(stage.command[6].indexOf('source_space_wait') < stage.command[6].indexOf('capacity_backup'));
  assert.doesNotMatch(stage.command[6], /actual_backup_fs_uuid=/);
  assert.equal(exportOptions, '-c default_transaction_read_only=on -c lock_timeout=5s');
  assert.ok(stage.command[6].includes("SELECT setting FROM pg_settings WHERE name = 'temp_file_limit'"));
  assert.doesNotMatch(stage.command[6], /(?:SET\s+temp_file_limit|GRANT\s+SET|ALTER\s+(?:ROLE|SYSTEM))/i);
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
  assert.equal(restore.env.find((entry: any) => entry.name === 'POD_UID').valueFrom.fieldRef.fieldPath, 'metadata.uid');
  assert.equal(restore.env.find((entry: any) => entry.name === 'BACKUP_UUID_ATTESTATION').value, 'external-csi-lease');
  assert.equal(restore.env.find((entry: any) => entry.name === 'SCRATCH_UUID_ATTESTATION').value, 'external-csi-lease');
  assert.equal(restore.env.find((entry: any) => entry.name === 'EXPECTED_SCRATCH_DEVICE').value, '');
  assert.deepEqual(restorePod.volumes.find((volume: any) => volume.name === 'tmp').emptyDir, { medium: 'Memory', sizeLimit: '64Mi' });
  assert.ok(restore.command[2].includes('setsid /bin/sh -ec'));
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
  *'/scratch') fixture_path=/scratch; fixture_uuid=fixture-scratch; fixture_device=/dev/fixture-scratch; fixture_number=8:33 ;;
  *) fixture_path=/backup; fixture_uuid=fixture-backup; fixture_device=/dev/fixture; fixture_number=8:32 ;;
esac
case "$*" in
  *FSTYPE*) if test -e /tmp/wrong-type; then printf overlay; else printf ext4; fi ;;
  *FSROOT*) if test -e /tmp/subdirectory; then printf /unbounded-local-path; else printf /; fi ;;
  *TARGET*) printf %s "$fixture_path" ;;
  *UUID*) if test ! -e /tmp/empty-uuid; then printf %s "$fixture_uuid"; fi ;;
  *MAJ:MIN*) printf %s "$fixture_number" ;;
  *SOURCE*) printf %s "$fixture_device" ;;
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
  await context.test('rootless copy guardian stops when its real 45-second lease expires and preserves partial bytes', { timeout: 65_000 }, async () => {
    const candidate = fixture();
    const path = shell(candidate, 'printf %s "$PATH"').toString();
    shell(candidate, `touch /tmp/empty-uuid; mkdir -p ${archiveDirectory}; printf preserved > ${archiveDirectory}/${archiveName}.partial`);
    write(candidate, '/tmp/backup-volume.lease', `${Math.floor(Date.now() / 1000)} fixture-pod /dev/fixture fixture-backup 8:32`);
    const started = performance.now();
    const child = spawn('docker', ['exec', '--env', `PATH=/tmp/bin:${path}`, '--env', 'BACKUP_UUID_ATTESTATION=external-csi-lease',
      '--env', 'EXPECTED_BACKUP_DEVICE=/dev/fixture', '--env', 'POD_UID=fixture-pod', candidate, '/bin/sh', '-ec', copyContainerCommand], { stdio: 'ignore' });
    const completed = new Promise<number | null>((resolve, reject) => { child.once('exit', resolve); child.once('error', reject); });
    try {
      await delay(2500);
      assert.equal(child.exitCode, null, 'Fresh CSI lease must allow the guardian to run');
      assert.notEqual(await completed, 0);
      const elapsed = performance.now() - started;
      assert.ok(elapsed >= 43_000 && elapsed < 60_000, `Actual lease expiry boundary changed: ${elapsed}`);
      assert.equal(shell(candidate, `cat ${archiveDirectory}/${archiveName}.partial`).toString(), 'preserved');
      shell(candidate, `test ! -e ${archiveDirectory}/OFFHOST_COPY_VERIFIED; test ! -e ${archiveDirectory}/LOCAL_ARCHIVE_CREATED; test ! -e /scratch/RESTORE_SUCCESS.json`);
    } finally {
      if (child.exitCode === null) child.kill('SIGKILL');
    }
  });
  const restoreEnvironment = {
    BACKUP_UUID_ATTESTATION: 'external-csi-lease', SCRATCH_UUID_ATTESTATION: 'external-csi-lease',
    EXPECTED_BACKUP_DEVICE: '/dev/fixture', EXPECTED_SCRATCH_DEVICE: '/dev/fixture-scratch', POD_UID: 'fixture-pod',
  };
  const restoreLeases = (candidate: string) => {
    shell(candidate, 'touch /tmp/empty-uuid');
    const epoch = Math.floor(Date.now() / 1000);
    write(candidate, '/tmp/backup-volume.lease', `${epoch} fixture-pod /dev/fixture fixture-backup 8:32`);
    write(candidate, '/tmp/scratch-volume.lease', `${epoch} fixture-pod /dev/fixture-scratch fixture-scratch 8:33`);
  };
  await context.test('rootless restore requires independent pod-bound backup and scratch attestations', () => {
    const candidate = fixture();
    const checkRestore = (extra: Record<string, string> = {}) => run(candidate, ['/bin/sh', '-ec', '. /policy/capacity.sh; capacity_restore'], { ...restoreEnvironment, ...extra });
    restoreLeases(candidate);
    checkRestore();
    for (const side of ['backup', 'scratch']) {
      const leasePath = `/tmp/${side}-volume.lease`;
      const valid = shell(candidate, `cat ${leasePath}`).toString();
      const fields = valid.split(' ');
      for (const [index, value] of [[0, '1'], [0, '9999999999'], [1, 'replaced-pod'], [2, '/dev/wrong'], [3, 'wrong-uuid'], [4, '8:34']] as const) {
        const invalid = [...fields];
        invalid[index] = value;
        write(candidate, leasePath, invalid.join(' '));
        assert.throws(() => checkRestore());
      }
      write(candidate, leasePath, valid + ' extra');
      assert.throws(() => checkRestore());
      shell(candidate, `rm ${leasePath}`);
      assert.throws(() => checkRestore());
      write(candidate, leasePath, valid);
    }
    assert.throws(() => checkRestore({ EXPECTED_SCRATCH_DEVICE: '/dev/fixture' }));
    assert.throws(() => checkRestore({ EXPECTED_SCRATCH_FS_UUID: 'fixture-backup' }));
    shell(candidate, 'cp /tmp/backup-volume.lease /tmp/scratch-volume.lease');
    assert.throws(() => checkRestore());
    shell(candidate, 'test ! -e /scratch/pgdata; test ! -e /scratch/RESTORE_SUCCESS.json');
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
  run(source, stageCommand(), { PGOPTIONS: exportOptions });
  const archive = shell(source, `cat ${archiveDirectory}/${archiveName}`);
  const expectedSha = createHash('sha256').update(archive).digest('hex');
  await context.test('ordinary read-only export role needs no parameter SET privilege and produces a full restorable archive', () => {
    const candidate = fixture();
    initializeSource(candidate);
    execute(candidate, ['psql', '-h', '/scratch/socket', '-U', 'postgres', '-d', 'source', '-v', 'ON_ERROR_STOP=1', '-c', 'CREATE ROLE export_reader LOGIN; GRANT pg_read_all_data TO export_reader;']);
    const database = 'host=/scratch/socket user=export_reader dbname=source';
    const query = "SELECT current_user, current_setting('transaction_read_only'), has_parameter_privilege(current_user, 'temp_file_limit', 'SET'), current_setting('temp_file_limit')";
    const command = ['psql', '--dbname=' + database, '-X', '-Atq', '-v', 'ON_ERROR_STOP=1', '-c', query];
    assert.equal(run(candidate, command, { PGOPTIONS: exportOptions }).toString().trim(), 'export_reader|on|f|-1');
    assert.throws(() => run(candidate, command, { PGOPTIONS: exportOptions + ' -c temp_file_limit=0' }), error => /permission denied to set parameter "temp_file_limit"/.test(String((error as { stderr?: Buffer }).stderr)));
    shell(candidate, `test ! -e ${archiveDirectory}; date +%s > /tmp/source-space.lease`);
    const output = run(candidate, stageCommand(), { DATABASE_URL: database, PGOPTIONS: exportOptions, BACKUP_RATE_MIB_PER_SECOND: '4' });
    assert.match(output.toString(), /source temp_file_limit_kib=-1 \(inherited; not modified\)/);
    shell(candidate, `cd ${archiveDirectory}; test -f LOCAL_ARCHIVE_CREATED; sha256sum -c ${archiveName}.sha256; pg_restore --list ${archiveName} >/dev/null`);
    execute(candidate, ['createdb', '-h', '/scratch/socket', '-U', 'postgres', 'restored']);
    execute(candidate, ['pg_restore', '-h', '/scratch/socket', '-U', 'postgres', '-d', 'restored', '--no-owner', '--no-privileges', '--exit-on-error', `${archiveDirectory}/${archiveName}`]);
    assert.equal(execute(candidate, ['psql', '-h', '/scratch/socket', '-U', 'postgres', '-d', 'restored', '-Atq', '-c', 'SELECT count(*), sum(octet_length(body)) FROM payload']).toString().trim(), '8192|8388608');
    assert.equal(execute(candidate, ['psql', '-h', '/scratch/socket', '-U', 'postgres', '-d', 'restored', '-Atq', '-c', "SELECT count(*) FROM pg_class relation JOIN pg_namespace namespace ON namespace.oid=relation.relnamespace WHERE namespace.nspname='public' AND relation.relkind='r'"]).toString().trim(), '254');
    assert.equal(run(candidate, command, { PGOPTIONS: exportOptions }).toString().trim(), 'export_reader|on|f|-1');
  });
  await context.test('actual prepared writer retains default pacing and bounds both reviewed faster rates', () => {
    const candidate = fixture();
    const payload = Buffer.alloc(16 * mib, 0x42);
    const digest = createHash('sha256').update(payload).digest('hex');
    write(candidate, '/tmp/rate-input', payload);
    const script = stage.command[6] as string;
    const start = script.indexOf('setsid prlimit --fsize=25769803776:25769803776');
    const end = script.indexOf('\nrate_pid=$!', start);
    assert.ok(start > 0 && end > start);
    const writer = `${exportRateSelection}\nexec 3>&1\narchive=/backup/rate-archive\nmkfifo /tmp/dump.pipe\n${script.slice(start, end)}\nrate_pid=$!\ncat /tmp/rate-input > /tmp/dump.pipe\nwait "$rate_pid"\nrm /tmp/dump.pipe\nsha256sum "$archive.partial"\n`;
    for (const rate of [undefined, '4', '8']) {
      const environment: Record<string, string> = rate === undefined ? {} : { BACKUP_RATE_MIB_PER_SECOND: rate };
      const started = performance.now();
      const output = run(candidate, ['/bin/sh', '-ec', writer], environment);
      const elapsed = performance.now() - started;
      const limit = Number(rate ?? '1');
      assert.ok(elapsed >= payload.length / (limit * mib) * 1000, `Writer exceeded ${limit}MiB/s pacing`);
      assert.equal(Number(shell(candidate, 'stat -c %s /backup/rate-archive.partial').toString()), payload.length);
      assert.equal(output.toString().split(' ')[0], digest);
      context.diagnostic(`Actual prepared writer: rate_MiBps=${limit} bytes=${payload.length} wall_ms=${Math.round(elapsed)} SHA matched`);
      shell(candidate, 'rm /backup/rate-archive.partial');
    }
    for (const rate of ['0', '2', '9', '64', '-1', '8;touch /tmp/injected-rate', 'unbounded']) {
      assert.throws(() => run(candidate, ['/bin/sh', '-ec', writer], { BACKUP_RATE_MIB_PER_SECOND: rate }));
      shell(candidate, 'test ! -e /tmp/injected-rate; test ! -e /tmp/dump.pipe; test ! -e /backup/rate-archive.partial');
    }
  });
  for (const rate of ['4', '8']) {
    await context.test(`reviewed ${rate}MiB/s export produces a fully restorable synthetic archive`, () => {
      const candidate = fixture();
      initializeSource(candidate);
      shell(candidate, 'date +%s > /tmp/source-space.lease');
      run(candidate, stageCommand(), { BACKUP_RATE_MIB_PER_SECOND: rate, PGOPTIONS: exportOptions });
      shell(candidate, `cd ${archiveDirectory}; test -f LOCAL_ARCHIVE_CREATED; sha256sum -c ${archiveName}.sha256; pg_restore --list ${archiveName} >/dev/null`);
      execute(candidate, ['createdb', '-h', '/scratch/socket', '-U', 'postgres', 'restored']);
      execute(candidate, ['pg_restore', '-h', '/scratch/socket', '-U', 'postgres', '-d', 'restored', '--no-owner', '--no-privileges', '--exit-on-error', `${archiveDirectory}/${archiveName}`]);
      const rows = execute(candidate, ['psql', '-h', '/scratch/socket', '-U', 'postgres', '-d', 'restored', '-Atq', '-v', 'ON_ERROR_STOP=1', '-c', 'SELECT count(*), sum(octet_length(body)) FROM payload']).toString().trim();
      assert.equal(rows, '8192|8388608');
      const relations = execute(candidate, ['psql', '-h', '/scratch/socket', '-U', 'postgres', '-d', 'restored', '-Atq', '-v', 'ON_ERROR_STOP=1', '-c', "SELECT count(*) FROM pg_class relation JOIN pg_namespace namespace ON namespace.oid=relation.relnamespace WHERE namespace.nspname='public' AND relation.relkind='r'"]).toString().trim();
      assert.equal(relations, '254');
    });
  }
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
  for (const mode of ['backup-revoked', 'scratch-revoked', 'parent-term'] as const) {
    await context.test(`${mode} stops an in-flight restore and its descendants without a success receipt`, { timeout: 45_000 }, async () => {
      const candidate = fixture();
      seed(candidate);
      restoreLeases(candidate);
      const actualRestore = shell(candidate, 'command -v pg_restore').toString().trim();
      write(candidate, '/tmp/bin/pg_restore', `#!/bin/sh\ncase "$*" in *--list*) exec ${actualRestore} "$@" ;; esac\ntrap '' TERM\nprintf '%s\\n' "$$" > /tmp/blocked-restore.pid\nsleep 100 &\nprintf '%s\\n' "$!" > /tmp/blocked-descendant.pid\nwait\n`);
      shell(candidate, 'chmod 700 /tmp/bin/pg_restore');
      const path = shell(candidate, 'printf %s "$PATH"').toString();
      const environment = { ...restoreEnvironment, PGOPTIONS: '', EXPECTED_SOURCE_SHA256: expectedSha };
      const command = [...restore.command];
      command[2] = 'printf \'%s\\n\' "$$" > /tmp/restore-supervisor.pid\n' + command[2];
      const child = spawn('docker', ['exec', '--env', `PATH=/tmp/bin:${path}`, ...Object.entries(environment).flatMap(([name, value]) => ['--env', `${name}=${value}`]), candidate, ...command], { stdio: 'ignore' });
      const completed = new Promise<number | null>((resolve, reject) => { child.once('exit', resolve); child.once('error', reject); });
      try {
        let active = false;
        for (let attempt = 0; attempt < 100 && child.exitCode === null; attempt++) {
          if (shell(candidate, 'if test -s /tmp/blocked-descendant.pid; then printf active; fi').toString() === 'active') { active = true; break; }
          await delay(100);
        }
        assert.ok(active, 'Must revoke during actual restore, not preflight');
        shell(candidate, 'pg_ctl -D /scratch/pgdata status');
        const stopped = performance.now();
        shell(candidate, mode === 'parent-term' ? 'kill -TERM "$(cat /tmp/restore-supervisor.pid)"' : `rm /tmp/${mode.split('-')[0]}-volume.lease`);
        assert.notEqual(await completed, 0);
        const elapsed = performance.now() - stopped;
        assert.ok(elapsed < 15_000, `Restore cleanup exceeded bound: ${elapsed}`);
        shell(candidate, 'test ! -e /scratch/RESTORE_SUCCESS.json; test -d /scratch/pgdata');
        assert.throws(() => shell(candidate, 'pg_ctl -D /scratch/pgdata status'));
        shell(candidate, `cd ${archiveDirectory}; sha256sum -c ${archiveName}.sha256; test -f OFFHOST_COPY_VERIFIED`);
        for (const pidFile of ['blocked-restore.pid', 'blocked-descendant.pid']) {
          const state = shell(candidate, `owned_pid=$(cat /tmp/${pidFile}); if test -f /proc/$owned_pid/stat; then awk '{print $3}' /proc/$owned_pid/stat; else printf gone; fi`).toString().trim();
          assert.ok(['gone', 'Z'].includes(state), `Owned restore descendant remains alive: ${state}`);
        }
        context.diagnostic(`Actual isolated restore cancellation: reason=${mode} wall_ms=${Math.round(elapsed)} no-success archive-preserved`);
      } finally {
        if (child.exitCode === null) child.kill('SIGKILL');
      }
    });
  }
  await context.test('two rootless CSI leases allow a real isolated restore but never ownership or application acceptance', () => {
    const candidate = fixture();
    seed(candidate);
    restoreLeases(candidate);
    run(candidate, restore.command, { ...restoreEnvironment, PGOPTIONS: '', EXPECTED_SOURCE_SHA256: expectedSha });
    const receipt = JSON.parse(shell(candidate, 'cat /scratch/RESTORE_SUCCESS.json').toString());
    assert.equal(receipt.archive_sha256, expectedSha);
    assert.equal(receipt.archive_bytes, archive.length);
    assert.equal(receipt.public_relations, 254);
    assert.equal(receipt.backup_fs_uuid, 'fixture-backup');
    assert.equal(receipt.scratch_fs_uuid, 'fixture-scratch');
    assert.equal(receipt.original_ownership_acl_verified, false);
    assert.equal(receipt.application_acceptance_verified, false);
    assert.equal(receipt.physical_wal_protection_verified, false);
    assert.throws(() => shell(candidate, 'pg_ctl -D /scratch/pgdata status'));
  });
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
