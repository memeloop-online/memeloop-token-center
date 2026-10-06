import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { capacityPolicy } from './capacity-policy.ts';
import { boundedJobs, preparedResources } from './hard-capacity.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const image = 'ghcr.io/cloudnative-pg/postgresql:17.5';
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;
const docker = (args: string[], input?: string) => execFileSync('docker', args, { input, encoding: 'utf8', timeout: 30_000 });
const shell = (container: string, script: string) => docker(['exec', container, '/bin/sh', '-ec', script]);
const write = (container: string, path: string, contents: string) => docker(['exec', '-i', container, 'dd', `of=${path}`, 'status=none'], contents);
const directory = '/backup/mtc-pg-logical-20261004';

function fixture(init: boolean, legacy = false): string {
  const stage = preparedResources(legacy ? script => script : undefined).find(resource => resource.kind === 'Job' && resource.metadata.name === boundedJobs.stage).spec.template.spec.containers[0];
  const environment = Object.fromEntries(stage.env.filter((entry: any) => entry.value !== undefined).map((entry: any) => [entry.name, entry.value]));
  Object.assign(environment, {
    PARENT_REVIEW_APPROVED: 'true', SOURCE_IO_REVIEW_APPROVED: 'true', ROOT_STORAGE_REVIEW_APPROVED: 'true', HARD_CAPACITY_REVIEW_APPROVED: 'true',
    EXPECTED_BACKUP_FS_UUID: 'fixture-backup', EXPECTED_BACKUP_DEVICE: '/dev/fixture', POD_UID: 'fixture-pod', EXPECTED_SERVER_ADDRESS: '127.0.0.1',
    BACKUP_MAX_BYTES: String(64 * 1024 ** 2), BACKUP_MIN_BYTES: '1048576', BACKUP_RESERVE_BYTES: '0', CAPACITY_MIN_FREE_INODES: '1',
    DATABASE_URL: 'unused-fixture-no-credentials', PGOPTIONS: '',
  });
  const container = docker(['run', '-d', '--network=none', '--read-only', '--user=26:26', '--cap-drop=ALL', '--security-opt=no-new-privileges',
    ...Object.entries(environment).flatMap(([name, value]) => ['--env', `${name}=${value}`]),
    '--tmpfs', '/tmp:rw,exec,size=32m,uid=26,gid=26', '--tmpfs', '/policy:rw,size=1m,uid=26,gid=26', '--tmpfs', '/backup:rw,size=64m,uid=26,gid=26',
    '--entrypoint=/bin/sh', image, '-ec', init ? 'while test ! -e /tmp/start; do sleep 0.1; done; exec /tmp/export-command' : 'exec sleep 300']).trim();
  try {
    shell(container, 'mkdir /tmp/bin');
    write(container, '/policy/capacity.sh', capacityPolicy);
    write(container, '/tmp/bin/findmnt', String.raw`#!/bin/sh
if test -e /tmp/findmnt-slow; then trap '' TERM; sleep 120; fi
case "$*" in
  *'/tmp') printf tmpfs ;;
  *FSTYPE*) printf xfs ;;
  *FSROOT*) printf / ;;
  *TARGET*) printf /backup ;;
  *UUID*) printf '' ;;
  *MAJ:MIN*) printf 8:32 ;;
  *SOURCE*) printf /dev/fixture ;;
  *) exit 1 ;;
esac
`);
    write(container, '/tmp/bin/psql', '#!/bin/sh\ncase "$*" in *"SELECT NOT pg_is_in_recovery()"*) printf "t\\n" ;; esac\n');
    write(container, '/tmp/bin/pg_dump', '#!/bin/sh\ntrap "" TERM\nprintf "%s\\n" "$$" > /tmp/producer.pid\ndd if=/dev/zero bs=65536 status=none &\nprintf "%s\\n" "$!" > /tmp/producer-child.pid\nwait "$!"\n');
    const command = [...stage.command];
    command[6] = command[6].replace('26071793664', '1048576');
    write(container, '/tmp/export-command', `#!/bin/sh\nexport PATH=/tmp/bin:$PATH\nexec ${command.map(quote).join(' ')}\n`);
    shell(container, 'chmod 700 /tmp/bin/* /tmp/export-command; date +%s > /tmp/source-space.lease');
    shell(container, 'printf "%s fixture-pod /dev/fixture fixture-backup 8:32\\n" "$(date +%s)" > /tmp/backup-volume.lease');
    return container;
  } catch (error) {
    docker(['rm', '-f', container]);
    throw error;
  }
}

async function active(container: string): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (shell(container, `if test -s ${directory}/memeloop_token_center.dump.partial; then printf active; fi`) === 'active') return;
    await delay(100);
  }
  assert.fail('Actual producer and rate writer did not start');
}

test('incident baseline: original background guard/parent-wait interaction at PID 1', { timeout: 45_000 }, async context => {
  const container = fixture(true, true);
  context.after(() => docker(['rm', '-f', container]));
  shell(container, 'touch /tmp/start');
  await active(container);
  context.diagnostic(`Original active process tree: ${docker(['top', container, '-eo', 'pid,ppid,pgid,stat,comm']).trim()}`);
  shell(container, 'rm /tmp/source-space.lease /tmp/backup-volume.lease');
  let stopped = false;
  for (let attempt = 0; attempt < 80; attempt++) {
    if (docker(['inspect', '-f', '{{.State.Running}}', container]).trim() === 'false') { stopped = true; break; }
    await delay(100);
  }
  context.diagnostic(`Original guard after lease revocation: stopped=${stopped}`);
  if (!stopped) {
    context.diagnostic(docker(['top', container, '-eo', 'pid,ppid,pgid,stat,comm']).trim());
    context.diagnostic(shell(container, 'printf "source_lease_present="; test ! -e /tmp/source-space.lease; printf "false\\n"; cat /tmp/stage.stdout /tmp/stage.stderr'));
  }
});

test('foreground expiry supervisor kills actual producer/writer groups without a live agent', { timeout: 240_000 }, async context => {
  for (const mode of ['source-revoked', 'volume-revoked', 'volume-expired', 'watcher-lost', 'capacity-timeout', 'nested-shell'] as const) {
    await context.test(mode, { timeout: 40_000 }, async childContext => {
      const init = mode !== 'nested-shell';
      const container = fixture(init);
      childContext.after(() => docker(['rm', '-f', container]));
      let exported: ReturnType<typeof spawn> | undefined;
      let completed: Promise<number | null> | undefined;
      if (init) shell(container, 'touch /tmp/start');
      else {
        exported = spawn('docker', ['exec', container, '/tmp/export-command'], { stdio: 'ignore' });
        completed = new Promise((resolve, reject) => { exported!.on('exit', resolve); exported!.on('error', reject); });
        childContext.after(() => { if (exported?.exitCode === null) exported.kill(); });
      }
      await active(container);
      const processes = docker(['top', container, '-eo', 'pid,ppid,pgid,stat,comm']);
      childContext.diagnostic(`Active producer/writer process tree: ${processes.trim()}`);
      assert.match(processes, /dd/);
      const started = Date.now();
      if (mode === 'volume-revoked') shell(container, 'rm /tmp/backup-volume.lease');
      else if (mode === 'volume-expired') shell(container, 'printf "%s fixture-pod /dev/fixture fixture-backup 8:32\\n" "$(($(date +%s) - 44))" > /tmp/backup-volume.lease');
      else if (mode === 'capacity-timeout') shell(container, 'touch /tmp/findmnt-slow');
      else if (mode === 'watcher-lost') shell(container, 'printf "%s\\n" "$(($(date +%s) - 44))" > /tmp/source-space.lease');
      else shell(container, 'rm /tmp/source-space.lease');
      let stopped = false;
      for (let attempt = 0; attempt < 150; attempt++) {
        if (init ? docker(['inspect', '-f', '{{.State.Running}}', container]).trim() === 'false' : exported!.exitCode !== null) { stopped = true; break; }
        await delay(100);
      }
      assert.ok(stopped, 'Lease expiry / blocked check must stop producer and writer within 15 seconds');
      assert.ok(Date.now() - started < 20_000);
      if (init) {
        assert.notEqual(Number(docker(['inspect', '-f', '{{.State.ExitCode}}', container]).trim()), 0);
        assert.equal(Number(docker(['inspect', '-f', '{{.State.Pid}}', container]).trim()), 0);
        const logs = docker(['logs', container]);
        assert.match(logs, /guard_abort=/);
        assert.doesNotMatch(logs, /local archive SHA256 and TOC only/);
      } else {
        assert.notEqual(await completed, 0);
        shell(container, `test -s ${directory}/memeloop_token_center.dump.partial; test ! -e ${directory}/LOCAL_ARCHIVE_CREATED`);
        const remaining = docker(['top', container, '-eo', 'pid,ppid,pgid,stat,comm']).trim().split('\n').slice(1).filter(line => !line.trim().split(/\s+/)[3]!.startsWith('Z'));
        assert.ok(remaining.every(line => line.trim().split(/\s+/).at(-1) === 'sleep'), `Leaked producer/writer: ${remaining.join('; ')}`);
      }
    });
  }
});
