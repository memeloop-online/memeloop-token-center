import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';
import { parseAllDocuments } from 'yaml';
import { archiveDirectory, archiveLimit, archiveName, copyArchive, copyChunk, copyFailureReceipt, type CopyStep, type Remote } from './copy.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const directory = dirname(fileURLToPath(import.meta.url));
const names = ['mtc-pg-local-stage-20261004.yaml', 'mtc-pg-offhost-copy-20261004.yaml', 'mtc-pg-restore-verification-20261004.yaml'];
const resources = names.flatMap(name => parseAllDocuments(readFileSync(join(directory, name), 'utf8')).map(document => {
  assert.deepEqual(document.errors, []);
  return document.toJS({ maxAliasCount: 0 });
}));
const job = (name: string) => resources.find(resource => resource.kind === 'Job' && resource.metadata.name === name);
const stage = job('mtc-pg-local-stage-20261004');
const restore = job('mtc-pg-restore-verification-20261004');
const stageContainer = stage.spec.template.spec.containers[0];
const restoreContainer = restore.spec.template.spec.containers[0];
const image = 'ghcr.io/cloudnative-pg/postgresql:17.5';
const containers: string[] = [];
const docker = (args: string[], input?: Buffer | string) => execFileSync('docker', args, { input, timeout: 180_000, maxBuffer: 20 * 1024 * 1024 });
const execute = (container: string, command: string[], input?: Buffer | string) => docker(['exec', ...(input === undefined ? [] : ['-i']), container, ...command], input);
const shell = (container: string, script: string) => execute(container, ['/bin/sh', '-ec', script]);
const write = (container: string, path: string, data: string | Buffer) => execute(container, ['dd', `of=${path}`, 'status=none'], data);

function structure(value: any[]): void {
  for (const resource of value.filter(entry => entry.kind === 'Job')) {
    assert.equal(resource.spec.suspend, true);
    assert.equal(resource.spec.backoffLimit, 0);
    const pod = resource.spec.template.spec;
    assert.equal(pod.restartPolicy, 'Never');
    assert.equal(pod.automountServiceAccountToken, false);
    assert.equal(pod.securityContext.runAsUser, 26);
    assert.equal(pod.securityContext.seccompProfile.type, 'RuntimeDefault');
    assert.equal(pod.hostNetwork, undefined);
    assert.equal(pod.hostPID, undefined);
    const container = pod.containers[0];
    assert.equal(container.securityContext.allowPrivilegeEscalation, false);
    assert.equal(container.securityContext.readOnlyRootFilesystem, true);
    assert.deepEqual(container.securityContext.capabilities, { drop: ['ALL'] });
  }
  const exportJob = value.find(entry => entry.metadata.name === 'mtc-pg-local-stage-20261004' && entry.kind === 'Job');
  const pod = exportJob.spec.template.spec;
  assert.equal(pod.nodeSelector['kubernetes.io/hostname'], 'haixia');
  const container = pod.containers[0];
  assert.deepEqual(container.command.slice(0, 6), ['/usr/bin/prlimit', '--core=0:0', '--fsize=16777216:25769803776', '--', '/bin/sh', '-ec']);
  const script = container.command[6];
  for (const required of ['--fsize=25769803776:25769803776', '--fsize=16777216:16777216', '--fsize=4096:4096', '--compress=none', 'dd bs=65536 count=1 iflag=fullblock', 'sleep 0.0625', 'timeout 5 df', '35651584', '67108864', 'wait "$dump_pid"', 'wait "$rate_pid"', 'head -c 4096 /tmp/stage.stdout', 'head -c 4096 /tmp/stage.stderr']) {
    assert.ok(script.includes(required), `Missing bound or failure propagation: ${required}`);
  }
  for (const gate of ['PARENT_REVIEW_APPROVED', 'SOURCE_IO_REVIEW_APPROVED', 'ROOT_STORAGE_REVIEW_APPROVED']) {
    assert.equal(container.env.find((entry: any) => entry.name === gate).value, 'false');
  }
  assert.deepEqual(pod.volumes.find((volume: any) => volume.name === 'tmp').emptyDir, { medium: 'Memory', sizeLimit: '64Mi' });
  const restorePod = value.find(entry => entry.kind === 'Job' && entry.metadata.name === 'mtc-pg-restore-verification-20261004').spec.template.spec;
  assert.equal(restorePod.nodeSelector['kubernetes.io/hostname'], 'versetensor-hv');
  assert.equal(restorePod.volumes.find((volume: any) => volume.name === 'backup').persistentVolumeClaim.readOnly, true);
  const verify = restorePod.containers[0];
  assert.equal(verify.env.find((entry: any) => entry.name === 'EXPECTED_SOURCE_SHA256').value, '');
  assert.ok(verify.command[2].includes('--no-owner --no-privileges --exit-on-error'));
  assert.ok(verify.command[2].includes('OFFHOST_COPY_VERIFIED'));
  assert.ok(verify.command[2].includes("listen_addresses=''"));
}

test('reviewed contracts and negative mutations fail closed independently of provenance', () => {
  const provenance = JSON.parse(readFileSync(join(directory, 'provenance.json'), 'utf8'));
  assert.equal(provenance.commit, 'ee49af94b76ac1016b213c4f12f36a60f84faaf8');
  for (const name of names) {
    assert.equal(createHash('sha256').update(readFileSync(join(directory, name))).digest('hex'), provenance.files[name]);
  }
  structure(resources);
  for (const mutate of [
    (value: any[]) => { value.find(entry => entry.kind === 'Job').spec.suspend = false; },
    (value: any[]) => { value.find(entry => entry.kind === 'Job').spec.template.spec.hostNetwork = true; },
    (value: any[]) => { value.find(entry => entry.kind === 'Job').spec.template.spec.containers[0].command[6] = stageContainer.command[6].replace('sleep 0.0625', 'true'); },
    (value: any[]) => { value.find(entry => entry.kind === 'Job').spec.template.spec.containers[0].command[6] = stageContainer.command[6].replace('35651584', '0'); },
    (value: any[]) => { value.find(entry => entry.kind === 'Job').spec.template.spec.containers[0].command[6] = stageContainer.command[6].replaceAll('wait "$rate_pid"', 'true'); },
  ]) {
    const changed = structuredClone(resources);
    mutate(changed);
    assert.throws(() => structure(changed));
  }
  execFileSync('/tmp/kubeconform', ['-strict', '-summary', '-exit-on-error', ...names.map(name => join(directory, name))], { timeout: 90_000 });
});

test('copy failure diagnostics retain the operation and exit category, never command output or credentials', async () => {
  const secret = 'private-archive-data-and-token';
  const failure = Object.assign(new Error(secret), { code: 'ETIMEDOUT', status: 1, signal: 'SIGTERM', stdout: Buffer.from(secret), stderr: Buffer.from(secret), command: secret });
  let step: CopyStep | undefined;
  await assert.rejects(copyArchive(() => { throw failure; }, '0'.repeat(64), undefined, false, archiveDirectory, 1, current => { step = current; }), error => error === failure);
  assert.deepEqual(step, { stage: 'source_checksum', side: 'source' });
  const receipt = copyFailureReceipt(failure, step);
  assert.equal(receipt.stage, 'source_checksum');
  assert.equal(receipt.exit_code, 1);
  assert.equal(receipt.code, 'ETIMEDOUT');
  assert.equal(receipt.signal, 'SIGTERM');
  assert.equal(receipt.stdout_bytes, Buffer.byteLength(secret));
  assert.equal(receipt.stderr_bytes, Buffer.byteLength(secret));
  assert.equal(receipt.archive_verified, false);
  assert.equal(receipt.automatic_retry, false);
  assert.equal(copyFailureReceipt(failure, { ...step!, checkpoint: 'container_identity' }).checkpoint, 'container_identity');
  assert.doesNotMatch(JSON.stringify(receipt), /private-archive-data|token|command|stack|message/);
  for (const reason of [null, secret, { message: secret, name: secret, code: secret, signal: secret, status: secret, stdout: secret, stderr: secret }]) {
    const unknown = copyFailureReceipt(reason, { stage: secret, side: secret, checkpoint: secret } as unknown as CopyStep);
    assert.equal(unknown.stage, 'preparation');
    assert.equal(unknown.side, null);
    assert.equal(unknown.checkpoint, null);
    assert.equal(unknown.code, null);
    assert.equal(unknown.exit_code, null);
    assert.doesNotMatch(JSON.stringify(unknown), /private-archive-data|token/);
  }
});

test('invalid copy rates fail before any remote operation', async () => {
  let calls = 0;
  const remote: Remote = () => { calls++; throw new Error('No remote operation expected'); };
  for (const rate of [0, -1, 2, 4.5, 16, Number.NaN, Number.POSITIVE_INFINITY]) {
    await assert.rejects(copyArchive(remote, '0'.repeat(64), undefined, false, archiveDirectory, rate), /Copy rate must/);
  }
  assert.equal(calls, 0);
});

function createContainer(): string {
  const container = docker(['run', '-d', '--network=none', '--read-only', '--user=26:26', '--cap-drop=ALL', '--security-opt=no-new-privileges',
    '--tmpfs', '/tmp:rw,exec,size=64m,uid=26,gid=26', '--tmpfs', '/backup:rw,size=128m,uid=26,gid=26', '--tmpfs', '/scratch:rw,size=256m,uid=26,gid=26',
    '--entrypoint=/bin/sleep', image, '1200']).toString().trim();
  containers.push(container);
  shell(container, 'mkdir /tmp/bin');
  const header = '#!' + '/bin/sh\nset -eu\n';
  write(container, '/tmp/bin/df', header + 'if test -f /tmp/df-slow; then sleep 20; fi\nif test -f /tmp/df-fail; then exit 1; fi\ncase "$*" in */tmp*) free=65536 ;; *) free=$(cat /tmp/free-kib) ;; esac\nprintf "Filesystem 1024-blocks Used Available Capacity Mounted\\nfixture 65536 0 %s 0%% /\\n" "$free"\n');
  write(container, '/tmp/free-kib', '67108864');
  write(container, '/tmp/bin/findmnt', header + 'case "$*" in *UUID*) printf test-nvme ;; *) printf tmpfs ;; esac\n');
  shell(container, 'chmod 700 /tmp/bin/*');
  assert.equal(shell(container, '/tmp/bin/findmnt -n -o UUID -T /backup').toString(), 'test-nvme');
  return container;
}

function initializeSource(container: string): void {
  const actualPsql = shell(container, 'command -v psql').toString().trim();
  shell(container, 'mkdir /scratch/socket; initdb -D /scratch/source -U postgres --auth-local=trust --auth-host=reject >/tmp/init.log; pg_ctl -D /scratch/source -l /tmp/postgres.log -o "-c listen_addresses= -c unix_socket_directories=/scratch/socket" -w start');
  execute(container, ['createdb', '-h', '/scratch/socket', '-U', 'postgres', 'source']);
  execute(container, [actualPsql, '-h', '/scratch/socket', '-U', 'postgres', '-d', 'source', '-v', 'ON_ERROR_STOP=1', '-c', "DO $$ BEGIN FOR counter IN 1..253 LOOP EXECUTE format('CREATE TABLE public.fixture_%s (id integer)', counter); END LOOP; END $$; CREATE TABLE public.payload AS SELECT entry AS id, repeat(md5(entry::text), 32) AS body FROM generate_series(1,8192) entry;"]);
  write(container, '/tmp/bin/psql', '#!' + '/bin/sh\ncase "$*" in *"SELECT NOT pg_is_in_recovery()"*) printf "t\\n" ;; *) exec ' + actualPsql + ' "$@" ;; esac\n');
  shell(container, 'chmod 700 /tmp/bin/psql');
}

function stageArgs(container: string): string[] {
  const path = shell(container, 'printf %s "$PATH"').toString();
  return ['exec', '--env', `PATH=/tmp/bin:${path}`, '--env', 'PARENT_REVIEW_APPROVED=true', '--env', 'SOURCE_IO_REVIEW_APPROVED=true', '--env', 'ROOT_STORAGE_REVIEW_APPROVED=true',
    '--env', 'EXPECTED_BACKUP_FS_UUID=test-nvme', '--env', 'EXPECTED_SERVER_ADDRESS=127.0.0.1', '--env', 'DATABASE_URL=host=/scratch/socket user=postgres dbname=source',
    '--env', 'PGOPTIONS=-c default_transaction_read_only=on -c lock_timeout=5s', container, ...stageContainer.command];
}

test('real PostgreSQL export, byte bounds, disk abort, resumable copy and full restore in isolated containers', { timeout: 600_000 }, async context => {
  context.after(() => { for (const container of containers) docker(['rm', '-f', container]); });
  docker(['pull', image]);
  console.log(docker(['image', 'inspect', image, '--format', '{{json .RepoDigests}}']).toString());
  const source = createContainer();
  initializeSource(source);
  const started = performance.now();
  let output: Buffer;
  try {
    output = docker(stageArgs(source));
  } catch (error) {
    const diagnostic = stageArgs(source);
    diagnostic[diagnostic.length - 1] = 'printf "approval=%s source=%s storage=%s uuid=%s\\n" "$PARENT_REVIEW_APPROVED" "$SOURCE_IO_REVIEW_APPROVED" "$ROOT_STORAGE_REVIEW_APPROVED" "$EXPECTED_BACKUP_FS_UUID"; command -v findmnt; command -v df; findmnt -n -o UUID -T /backup; findmnt -n -o FSTYPE -T /tmp; df -Pk /tmp; df -Pk /backup; head -c 8192 /tmp/stage.stdout /tmp/stage.stderr 2>/dev/null || true';
    console.log(docker(diagnostic).toString());
    throw error;
  }
  const elapsed = performance.now() - started;
  const size = Number(shell(source, `stat -c %s ${archiveDirectory}/${archiveName}`).toString());
  assert.ok(size > copyChunk * 2);
  assert.ok(elapsed >= size / 1024 / 1024 * 1000, 'Archive output exceeded 1MiB/s pacing');
  assert.ok(output.length <= 8192, 'Emitted logs exceed 8KiB');
  const expectedSha = shell(source, `sha256sum ${archiveDirectory}/${archiveName}`).toString().split(' ')[0]!;
  assert.equal(expectedSha.length, 64);
  assert.throws(() => shell(source, `prlimit --core=0:0 --fsize=${archiveLimit}:${archiveLimit} -- dd if=/dev/zero of=/backup/overflow bs=1 seek=${archiveLimit} count=1 status=none`));
  assert.ok(Number(shell(source, 'stat -c %s /backup/overflow').toString()) <= archiveLimit);
  assert.throws(() => shell(source, 'prlimit --core=0:0 --fsize=16777216:16777216 -- dd if=/dev/zero of=/backup/toc-overflow bs=1048576 count=17 status=none'));
  assert.equal(Number(shell(source, 'stat -c %s /backup/toc-overflow').toString()), 16777216);
  assert.throws(() => shell(source, 'dd if=/dev/zero of=/tmp/tmp-overflow bs=1048576 count=65 status=none'));
  shell(source, 'rm /tmp/tmp-overflow');

  const destination = createContainer();
  const remote: Remote = (side, command, input) => {
    const target = side === 'source' ? source : destination;
    const path = shell(target, 'printf %s "$PATH"').toString();
    return execute(target, ['env', `PATH=/tmp/bin:${path}`, ...command], input);
  };
  let writes = 0;
  let failedStep: CopyStep | undefined;
  await assert.rejects(copyArchive((side, command, input) => {
    if (input && ++writes === 2) throw new Error('simulated interrupted transfer');
    return remote(side, command, input);
  }, expectedSha, async () => {}, false, archiveDirectory, 1, step => { failedStep = step; }), /interrupted transfer/);
  assert.deepEqual(failedStep, { stage: 'destination_chunk', side: 'destination' });
  assert.equal(Number(shell(destination, `stat -c %s ${archiveDirectory}/${archiveName}.partial`).toString()), copyChunk);
  shell(destination, `test ! -f ${archiveDirectory}/OFFHOST_COPY_VERIFIED`);
  const sleeps: number[] = [];
  const phases: CopyStep[] = [];
  await copyArchive(remote, expectedSha, async milliseconds => { sleeps.push(milliseconds!); }, false, archiveDirectory, 1, step => { phases.push(step); });
  assert.deepEqual(phases[0], { stage: 'source_checksum', side: 'source' });
  assert.deepEqual(phases.at(-1), { stage: 'destination_finalize', side: 'destination' });
  assert.ok(sleeps.every(milliseconds => milliseconds > 0 && milliseconds <= 4000));
  assert.equal(sleeps.reduce((total, milliseconds) => total + milliseconds, 0), (size - copyChunk) / 1024 / 1024 * 1000);
  shell(destination, `test -f ${archiveDirectory}/OFFHOST_COPY_VERIFIED`);
  assert.equal(shell(destination, `sha256sum ${archiveDirectory}/${archiveName}`).toString().split(' ')[0], expectedSha);
  const restoreOutput = docker(['exec', '--env', 'PARENT_REVIEW_APPROVED=true', '--env', `EXPECTED_SOURCE_SHA256=${expectedSha}`, destination, ...restoreContainer.command]);
  assert.match(restoreOutput.toString(), /full archive restore completed.*254/);
  console.log(`Uncompressed bytes=${size}; export wall_ms=${Math.round(elapsed)}; interrupted copy resumed; destination SHA=${expectedSha}; full isolated restore passed`);

  await context.test('incorrect source digest fails before transfer', async () => {
    await assert.rejects(copyArchive(remote, '0'.repeat(64), async () => {}));
  });
  await context.test('same-size corrupted destination cannot acquire completion marker', async () => {
    shell(destination, `cd ${archiveDirectory}; rm OFFHOST_COPY_VERIFIED; mv ${archiveName} ${archiveName}.partial; printf X | dd of=${archiveName}.partial conv=notrunc status=none`);
    await assert.rejects(copyArchive(remote, expectedSha, async () => {}));
    shell(destination, `test ! -f ${archiveDirectory}/OFFHOST_COPY_VERIFIED; test -f ${archiveDirectory}/${archiveName}.partial`);
  });

  for (const mode of ['watermark', 'df-failure', 'df-timeout'] as const) {
    await context.test(`${mode} aborts active export and retains partial`, async () => {
      const candidate = createContainer();
      initializeSource(candidate);
      const child = spawn('docker', stageArgs(candidate), { stdio: ['ignore', 'pipe', 'pipe'] });
      let emitted = 0;
      child.stdout.on('data', (bytes: Buffer) => { emitted += bytes.length; });
      child.stderr.on('data', (bytes: Buffer) => { emitted += bytes.length; });
      const completed = new Promise<number | null>((resolve, reject) => { child.on('error', reject); child.on('exit', resolve); });
      await delay(1000);
      if (mode === 'watermark') write(candidate, '/tmp/free-kib', '33554432');
      else shell(candidate, `touch /tmp/${mode === 'df-failure' ? 'df-fail' : 'df-slow'}`);
      assert.notEqual(await completed, 0);
      assert.ok(emitted <= 8192);
      shell(candidate, `test -f ${archiveDirectory}/${archiveName}.partial; test ! -f ${archiveDirectory}/LOCAL_ARCHIVE_CREATED`);
    });
  }
});
