import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { setTimeout as delay } from 'node:timers/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

// This is a small disposable GHA protocol fixture, never a production baseline.
assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Barman contracts run only in GitHub Actions');
const endpoint = 'http://127.0.0.1:19000';
const bucket = 'mtc-pg-recovery';
const prefix = '587427b2-ca1d-4497-8352-7e9897d205a7';
const destination = `s3://${bucket}/${prefix}`;
const server = 'mtc-prod-pg-587427b2';
const image = 'ghcr.io/cloudnative-pg/postgresql@sha256:b1deeed2aa998b2f381e39c5cadb9ec06127708c8bd62965743af19abf21628f';
const container = 'mtc-barman-rustfs-contract';
const access = 'pg-recovery-fixture';
const secret = 'synthetic-barman-fixture-only-secret';
const rootAccess = 'rustfs-test-fixture';
const rootSecret = 'synthetic-test-only-secret-707';
const mc = process.env.MTC_TEST_MC_BINARY;
assert.ok(mc, 'Use the existing checksum-pinned S3 admin client');
const directory = mkdtempSync(join(tmpdir(), 'barman-rustfs-contract-'));
const adminEnv = { ...process.env, MC_CONFIG_DIR: join(directory, 'mc') };
const run = (binary: string, args: string[], env = process.env, input?: string) =>
  execFileSync(binary, args, { env, input, encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 });
const docker = (args: string[], input?: string) => run('docker', args, process.env, input);
const execute = (...args: string[]) => docker(['exec', container, ...args]);
const admin = (...args: string[]) => run(mc, args, adminEnv);
const cloud = (...args: string[]) => execute(...args);
const cloudArgs = ['--endpoint-url', endpoint];
try {
  admin('alias', 'set', 'fixture', endpoint, rootAccess, rootSecret);
  admin('mb', '--ignore-existing', `fixture/${bucket}`);
  admin('anonymous', 'set', 'none', `fixture/${bucket}`);
  admin('admin', 'user', 'add', 'fixture', access, secret);
  admin('admin', 'policy', 'create', 'fixture', 'pg-recovery-fixture',
    fileURLToPath(new URL('./rustfs-barman-policy.json', import.meta.url)));
  admin('admin', 'policy', 'attach', 'fixture', 'pg-recovery-fixture', '--user', access);
  const signed = (path: string, method = 'GET') => run('curl', [
    '--silent', '--show-error', '--aws-sigv4', 'aws:amz:us-east-1:s3', '--user', `${access}:${secret}`,
    '--request', method, '--output', '/dev/null', '--write-out', '%{http_code}', `${endpoint}/${path}`,
  ]);
  assert.equal(signed(bucket), '200', 'Scoped identity can list only its recovery bucket');
  assert.equal(signed(`${bucket}/outside-the-uid/object`, 'PUT'), '403');
  assert.equal(signed('conversation-contract-test/forbidden', 'PUT'), '403');
  assert.equal(run('curl', ['--silent', '--output', '/dev/null', '--write-out', '%{http_code}', `${endpoint}/${bucket}`]), '403');
  docker(['run', '--detach', '--name', container, '--network', 'host', '--memory', '768m', '--cpus', '2',
    '--health-cmd', 'pg_isready -h /tmp -p 55432', '--health-interval', '1s', '--health-timeout', '2s', '--health-retries', '60',
    '--user', '26:26', '--env', `AWS_ACCESS_KEY_ID=${access}`, '--env', `AWS_SECRET_ACCESS_KEY=${secret}`,
    '--env', 'AWS_DEFAULT_REGION=us-east-1', '--env', 'AWS_EC2_METADATA_DISABLED=true',
    '--entrypoint', '/bin/bash', image, '-ec', `
      initdb -D /tmp/pg --no-locale --encoding=UTF8 --auth=trust >/dev/null
      cat >> /tmp/pg/postgresql.conf <<'CONFIG'
listen_addresses = ''
unix_socket_directories = '/tmp'
port = 55432
checkpoint_timeout = '30s'
checkpoint_completion_target = 0.1
archive_mode = on
archive_command = 'barman-cloud-wal-archive -z --endpoint-url ${endpoint} ${destination} ${server} %p'
CONFIG
      pg_ctl -D /tmp/pg -l /tmp/postgres.log -w start
      exec sleep infinity
    `]);
  let ready = false;
  for (let attempt = 0; attempt < 90; attempt += 1) {
    const health = docker(['inspect', '--format', '{{.State.Health.Status}}', container]).trim();
    if (health === 'healthy') { ready = true; break; }
    if (health === 'unhealthy') break;
    await delay(1_000);
  }
  assert.ok(ready, 'The isolated PG17 fixture must actually become ready');
  assert.match(execute('barman-cloud-backup', '--version'), /3\.14\.0/);
  execute('psql', '-h', '/tmp', '-p', '55432', '-U', 'postgres', '-d', 'postgres', '-v', 'ON_ERROR_STOP=1', '-c',
    "CREATE TABLE protocol_receipt (value text); INSERT INTO protocol_receipt VALUES ('isolated-ci-barman-rustfs');");
  cloud('barman-cloud-check-wal-archive', ...cloudArgs, destination, server);
  // Exercise the actual bundled Barman S3 implementation's multipart completion
  // and full download; no production bucket or root credentials reach Barman.
  execute('python3', '-c', `
from barman.cloud_providers.aws_s3 import S3CloudInterface
from io import BytesIO
from pathlib import Path
import hashlib
c = S3CloudInterface('${destination}', endpoint_url='${endpoint}', jobs=1)
key = '${prefix}/${server}/protocol/multipart.bin'
body = bytes(range(256)) * (6 * 1024 * 1024 // 256)
upload = c.create_multipart_upload(key)
parts = [c._upload_part(upload, key, BytesIO(body[:5*1024*1024]), 1), c._upload_part(upload, key, BytesIO(body[5*1024*1024:]), 2)]
c._complete_multipart_upload(upload, key, parts)
c.download_file(key, '/tmp/multipart.bin', None)
assert hashlib.sha256(Path('/tmp/multipart.bin').read_bytes()).digest() == hashlib.sha256(body).digest()
`);
  cloud('barman-cloud-backup', ...cloudArgs, '--gzip', '--jobs', '1', '--max-bandwidth=8388608',
    '-h', '/tmp', '-p', '55432', '-U', 'postgres', '-d', 'postgres', destination, server);
  const catalog = JSON.parse(cloud('barman-cloud-backup-list', ...cloudArgs, '--format', 'json', destination, server));
  const complete = catalog.backups_list.filter((entry: { status: string }) => entry.status === 'DONE');
  assert.equal(complete.length, 1, 'Real Barman metadata identifies one complete isolated baseline');
  const backupId = complete[0].backup_id;
  assert.match(backupId, /^\d{8}T\d{6}$/);
  // Extract the small real backup with Barman restore, then replay its archived
  // WAL in a distinct local data directory with no listener/production Service.
  cloud('barman-cloud-restore', ...cloudArgs, destination, server, backupId, '/tmp/recovered');
  execute('/bin/bash', '-ec', `
    cat >> /tmp/recovered/postgresql.conf <<'CONFIG'
port = 55433
archive_mode = off
restore_command = 'barman-cloud-wal-restore --endpoint-url ${endpoint} ${destination} ${server} %f %p'
recovery_target = 'immediate'
recovery_target_action = 'promote'
CONFIG
    touch /tmp/recovered/recovery.signal
    pg_ctl -D /tmp/recovered -l /tmp/recovered.log -w start || { cat /tmp/recovered.log; exit 1; }
  `);
  assert.equal(execute('psql', '-h', '/tmp', '-p', '55433', '-U', 'postgres', '-d', 'postgres', '-Atc',
    'SELECT value FROM protocol_receipt').trim(), 'isolated-ci-barman-rustfs');
  execute('python3', '-c', `
from barman.cloud_providers.aws_s3 import S3CloudInterface
c = S3CloudInterface('${destination}', endpoint_url='${endpoint}', jobs=1)
parents = list(c.list_bucket('${prefix}/${server}/wals/'))
keys = [key for parent in parents for key in (c.list_bucket(parent) if parent.endswith('/') else [parent])]
assert any('/wals/' in key and key.endswith('.gz') for key in keys), keys
`);
  console.log(JSON.stringify({ kind: 'isolated_barman_rustfs_contract', barman: '3.14.0', image,
    multipart_full_get: true, private_scoped_bucket: true, baseline_metadata: backupId,
    wal_archive_and_local_replay: true, production_restore_proven: false }));
} finally {
  try { docker(['rm', '--force', container]); } catch { /* It may not have been created. */ }
  rmSync(directory, { recursive: true, force: true });
}
