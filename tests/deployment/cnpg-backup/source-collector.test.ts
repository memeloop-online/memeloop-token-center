import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test, { type TestContext } from 'node:test';
import { fileURLToPath } from 'node:url';
import { sourceSpace } from './source-space.ts';
import { stageIdentity } from './volume-identity.ts';
import { sourceFilesystem, sourceStatCommand } from './source-filesystem.ts';
import { sourceInventory, sourceStatOutput } from './source-filesystem.fixture.ts';
import { capacityPolicy } from './capacity-policy.ts';
import { boundedJobs, preparedResources } from './hard-capacity.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const collector = fileURLToPath(new URL('./source-space.ts', import.meta.url));
const fixture = fileURLToPath(new URL('./collector-api-fixture.ts', import.meta.url));
const sentinel = 'SENSITIVE_API_BODY_SENTINEL';
const exportName = `${sourceSpace.job}-fixture`;
const exportPod = {
  metadata: { namespace: sourceSpace.namespace, uid: 'fixture-export', labels: { 'job-name': sourceSpace.job }, ownerReferences: [{ kind: 'Job', name: sourceSpace.job }] },
  spec: { nodeName: sourceSpace.node, volumes: [{ persistentVolumeClaim: { claimName: sourceSpace.stageClaim } }], containers: [{ name: 'export', env: [
    { name: 'EXPECTED_BACKUP_FS_UUID', value: stageIdentity.filesystemUUID },
    { name: 'EXPECTED_BACKUP_DEVICE', value: stageIdentity.device },
    { name: 'BACKUP_UUID_ATTESTATION', value: 'external-csi-lease' },
    { name: 'POD_UID', valueFrom: { fieldRef: { fieldPath: 'metadata.uid' } } },
  ] }] },
  status: { phase: 'Running' },
};
type Step = { contains: string[]; reply?: any; delayMs?: number; requestTimeout?: string; stderr?: string; exitCode?: number; leaseOperation?: 'publish' | 'revoke' };
function sourceCalls(options: { statDelayMs?: number; output?: string; after?: (value: any) => void } = {}): Step[] {
  const before = sourceInventory();
  const after = sourceInventory();
  options.after?.(after);
  return [
    { contains: ['get', 'clusters.postgresql.cnpg.io'], reply: before.cluster },
    { contains: ['get', 'pod', sourceSpace.pod], reply: before.pod },
    { contains: ['get', 'pvc', sourceSpace.claim], reply: before.claim },
    { contains: ['get', 'pv', sourceFilesystem.persistent], reply: before.persistent },
    { contains: ['exec', sourceSpace.pod, '-c', 'postgres', ...sourceStatCommand], requestTimeout: '--request-timeout=8s', reply: options.output ?? sourceStatOutput(), delayMs: options.statDelayMs ?? 0 },
    { contains: ['get', 'clusters.postgresql.cnpg.io'], reply: after.cluster },
    { contains: ['get', 'pvc', sourceSpace.claim], reply: after.claim },
    { contains: ['get', 'pv', sourceFilesystem.persistent], reply: after.persistent },
    { contains: ['get', 'pod', sourceSpace.pod], reply: after.pod },
  ];
}
const exportCall = (): Step => ({ contains: ['get', 'pod', exportName], reply: exportPod });
const volumeCalls = (): Step[] => [
  { contains: ['get', 'pvc'], reply: { metadata: { name: stageIdentity.name, uid: stageIdentity.claimUID, namespace: stageIdentity.namespace }, spec: { volumeName: stageIdentity.name }, status: { phase: 'Bound' } } },
  { contains: ['get', 'pv'], reply: { metadata: { name: stageIdentity.name, uid: stageIdentity.persistentUID }, status: { phase: 'Bound' }, spec: { claimRef: { name: stageIdentity.name, namespace: stageIdentity.namespace, uid: stageIdentity.claimUID }, volumeMode: 'Filesystem', persistentVolumeReclaimPolicy: 'Retain', capacity: { storage: '28Gi' }, csi: { driver: 'driver.longhorn.io', fsType: 'xfs', volumeHandle: stageIdentity.name } } } },
  { contains: ['get', 'volumes.longhorn.io'], reply: { metadata: { name: stageIdentity.name, uid: stageIdentity.longhornUID, namespace: 'longhorn-system' }, spec: { size: String(28 * 1024 ** 3), numberOfReplicas: 1, dataLocality: 'strict-local' }, status: { state: 'attached', robustness: 'healthy', currentNodeID: stageIdentity.node } } },
  { contains: ['get', 'replicas.longhorn.io'], reply: { items: [{ metadata: {}, spec: { volumeName: stageIdentity.name, nodeID: stageIdentity.node, diskID: stageIdentity.diskUUID, diskPath: stageIdentity.diskPath, failedAt: '' }, status: { currentState: 'running' } }] } },
  { contains: ['get', 'pods'], reply: { items: [{ metadata: { name: 'fixture-csi' }, spec: { nodeName: stageIdentity.node }, status: { phase: 'Running', containerStatuses: [{ name: 'longhorn-csi-plugin', ready: true }] } }] } },
  { contains: ['exec', 'fixture-csi', 'blkid', stageIdentity.device], reply: `UUID=${stageIdentity.filesystemUUID}\nTYPE=xfs\n` },
  { contains: ['exec', 'fixture-csi', 'stat', stageIdentity.device], reply: '8:20\n' },
];
const revokeCalls = (): Step[] => [exportCall(), { contains: ['exec', exportName, '/bin/rm', '/tmp/source-space.lease', '/tmp/backup-volume.lease'], reply: '' }];

async function run(context: TestContext, plan: Step[], watch = false, leaseContainer?: string) {
  const directory = mkdtempSync(join(tmpdir(), 'cnpg-collector-'));
  context.after(() => rmSync(directory, { recursive: true, force: true }));
  writeFileSync(join(directory, 'plan.json'), JSON.stringify(plan));
  writeFileSync(join(directory, 'state.json'), JSON.stringify({ index: 0 }));
  writeFileSync(join(directory, 'kubectl'), `#!/bin/sh\nexec '${process.execPath}' '${fixture}' "$@"\n`, { mode: 0o700 });
  const child = spawn(process.execPath, [collector, ...(watch ? ['--watch', exportName] : ['--check'])], {
    detached: true, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, PATH: `${directory}:${process.env.PATH}`, KUBECONFIG: join(directory, 'no-cluster-credentials'), COLLECTOR_FIXTURE_DIRECTORY: directory, PARENT_REVIEW_APPROVED: 'true', ...(leaseContainer ? { COLLECTOR_LEASE_CONTAINER: leaseContainer } : {}) },
  });
  const stop = () => { try { if (child.pid) process.kill(-child.pid, 'SIGKILL'); } catch { } };
  context.after(stop);
  const deadline = setTimeout(stop, 110_000);
  let stdout = '';
  let stderr = '';
  child.stdout!.on('data', chunk => { stdout += chunk; });
  child.stderr!.on('data', chunk => { stderr += chunk; });
  let status: number | null;
  try {
    status = await new Promise<number | null>((resolve, reject) => { child.on('error', reject); child.on('close', resolve); });
  } finally { clearTimeout(deadline); }
  assert.doesNotMatch(stdout + stderr, new RegExp(sentinel));
  const events = stderr.trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  assert.ok(events.filter(event => event.event === 'api-start').every(event => !['source-summary', 'api-other'].includes(event.phase)), 'No cached-summary fallback or unrecognized API operation');
  assert.equal(JSON.parse(readFileSync(join(directory, 'state.json'), 'utf8')).index, plan.length, stderr);
  for (const ended of events.filter(event => event.event === 'api-end' || event.event === 'collection-end')) {
    const started = events.find(event => event.event === ended.event.replace('-end', '-start') && event.cycle === ended.cycle && event.attempt === ended.attempt && event.call === ended.call);
    assert.ok(started, JSON.stringify(ended));
    assert.ok(ended.durationMs >= 0 && ended.monotonicMs >= started.monotonicMs);
    assert.ok(Number.isFinite(Date.parse(started.observedAt)) && Number.isFinite(Date.parse(ended.observedAt)));
  }
  context.diagnostic(JSON.stringify({ status, observations: events.filter(event => ['source-sample', 'lease-publication-start', 'lease-publication-end', 'transient-api-retry-no-lease-renewal', 'collector-failed'].includes(event.event)), delayedCalls: events.filter(event => event.event === 'api-end' && event.durationMs >= 200) }));
  const calls = readFileSync(join(directory, 'calls.jsonl'), 'utf8').trim().split('\n').map(line => JSON.parse(line));
  return { status, events, stdout, calls };
}

test('real collector executes a new stat and retains its conservative collection start through postflight', { timeout: 120_000 }, async context => {
  const result = await run(context, sourceCalls({ statDelayMs: 350 }));
  assert.equal(result.status, 0);
  const sample = JSON.parse(result.stdout);
  assert.equal(sample.method, 'mounted-statfs');
  assert.equal(sample.sourcePodUID, sourceFilesystem.podUID);
  assert.equal(sample.availableBytes, 10 * 1024 ** 3);
  assert.ok(Date.parse(sample.time) <= Date.parse(sample.execStartedAt));
  assert.ok(Date.parse(sample.execCompletedAt) - Date.parse(sample.execStartedAt) >= 350);
  assert.ok(Date.parse(sample.collectedAt) >= Date.parse(sample.execCompletedAt));
  assert.equal(sample.sourceSpace.maximumSampleAgeMs, 90000);
  assert.equal(sample.sourceSpace.leaseSeconds, 45);
  assert.ok(result.events.some(event => event.phase === 'source-statfs' && event.durationMs >= 350));
});

test('real stat/parser errors and mount races cannot fall back to a cached summary or leak its body', { timeout: 120_000 }, async context => {
  for (const output of [sentinel, sourceStatOutput().replace('15721786', '65535'), sourceStatOutput().replace(/65:48\n$/, '65:49\n')]) {
    const result = await run(context, sourceCalls({ output }).slice(0, 5));
    assert.equal(result.status, 1);
    assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
    assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 0);
  }
});

test('actual collector records rejected capacity before stopping and revoking without a new lease', { timeout: 120_000 }, async context => {
  const initial = 10 * 1024 ** 3;
  const available = initial - sourceSpace.maximumDropBytes;
  const result = await run(context, [
    ...sourceCalls(), exportCall(), ...volumeCalls(),
    ...sourceCalls({ output: sourceStatOutput(available / 4096) }), ...revokeCalls(),
  ], true);
  assert.equal(result.status, 1);
  const budget = result.events.findIndex(event => event.event === 'source-budget' && event.category === 'SOURCE_BUDGET_DROP');
  const stopping = result.events.findIndex(event => event.event === 'watcher-stopping-renewal');
  const revocation = result.events.findIndex(event => event.phase === 'lease-revoke' && event.outcome === 'success');
  assert.ok(budget >= 0 && stopping > budget && revocation > stopping);
  assert.equal(result.events[budget].availableBytes, available);
  assert.equal(result.events[budget].initialAvailableBytes, initial);
  assert.equal(result.events[budget].dropBytes, sourceSpace.maximumDropBytes);
  assert.equal(result.events[budget].minimumAvailableBytes, sourceSpace.stopBytes);
  assert.equal(result.events[budget].maximumDropBytes, 512 * 1024 ** 2);
  assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
  assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 0);
});

test('actual collector rejects pre/post source identity races before lease publication', { timeout: 300_000 }, async context => {
  for (const [name, mutate] of [
    ['pod', (value: any) => { value.pod.metadata.uid = 'replacement'; }],
    ['container', (value: any) => { value.pod.status.containerStatuses[0].containerID = `containerd://${'b'.repeat(64)}`; }],
    ['restart', (value: any) => { value.pod.status.containerStatuses[0].restartCount = 1; }],
    ['wal-mount', (value: any) => { value.pod.spec.containers[0].volumeMounts.push({ name: 'wal', mountPath: `${sourceFilesystem.pgdata}/pg_wal` }); }],
    ['base-mount', (value: any) => { value.pod.spec.containers[0].volumeMounts.push({ name: 'base', mountPath: `${sourceFilesystem.pgdata}/base` }); }],
    ['claim', (value: any) => { value.claim.metadata.uid = 'replacement'; }],
    ['pv', (value: any) => { value.persistent.metadata.uid = 'replacement'; }],
    ['claimRef', (value: any) => { value.persistent.spec.claimRef.uid = 'replacement'; }],
    ['volumeHandle', (value: any) => { value.persistent.spec.csi.volumeHandle = 'replacement'; }],
    ['node', (value: any) => { value.pod.spec.nodeName = 'other'; }],
    ['primary', (value: any) => { value.cluster.status.currentPrimary = 'other'; }],
  ] as const) {
    await context.test(name, async childContext => {
      const result = await run(childContext, [...sourceCalls(), exportCall(), ...volumeCalls(), ...sourceCalls({ after: mutate }), ...revokeCalls()], true);
      assert.equal(result.status, 1);
      assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
      assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 0);
      assert.ok(result.events.some(event => event.phase === 'lease-revoke' && event.outcome === 'success'));
    });
  }
});

test('real delayed identity subprocesses expire the stat observation after 90s without retimestamping', { timeout: 120_000 }, async context => {
  const plan = sourceCalls({ statDelayMs: 250 });
  for (const step of plan.slice(0, 4)) step.delayMs = 14000;
  for (const step of plan.slice(5)) step.delayMs = 9000;
  const result = await run(context, plan);
  assert.equal(result.status, 1);
  const sample = result.events.find(event => event.event === 'source-sample');
  assert.equal(sample.category, 'SOURCE_SAMPLE_STALE');
  assert.ok(sample.sampleAgeMs > 90000 && sample.monotonicAgeMs > 90000);
  assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
});

test('every renewal requires a new stat and an exec failure cannot reuse the prior fresh observation', { timeout: 120_000 }, async context => {
  const failed = sourceCalls().slice(0, 5);
  Object.assign(failed[4]!, { exitCode: 1, reply: sentinel, stderr: sentinel });
  const result = await run(context, [
    ...sourceCalls(), exportCall(), ...volumeCalls(), ...sourceCalls({ statDelayMs: 250 }),
    { contains: ['exec', exportName, '/bin/sh'], reply: '', delayMs: 350 },
    exportCall(), ...volumeCalls(), ...failed, ...revokeCalls(),
  ], true);
  assert.equal(result.status, 1);
  assert.equal(result.events.filter(event => event.event === 'api-start' && event.phase === 'source-statfs').length, 3);
  const published = result.events.filter(event => event.event === 'lease-publication-end');
  assert.equal(published.length, 1);
  assert.equal(published[0].leaseSeconds, 45);
  const publishing = result.events.find(event => event.event === 'lease-publication-start');
  assert.ok(published[0].sampleAgeMs >= publishing.sampleAgeMs + 350);
  assert.ok(result.events.some(event => event.phase === 'lease-revoke' && event.outcome === 'success'));
});

test('stat API timeout is ten seconds, a complete retry recollects identity and inner timeout fails closed', { timeout: 120_000 }, async context => {
  const blocked = sourceCalls().slice(0, 5);
  blocked[4]!.delayMs = 60000;
  const recovered = await run(context, [...blocked, ...sourceCalls()]);
  assert.equal(recovered.status, 0);
  const timeout = recovered.events.find(event => event.phase === 'source-statfs' && event.category === 'API_TIMEOUT');
  assert.ok(timeout.durationMs >= 10000 && timeout.durationMs < 15000);
  assert.equal(recovered.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 1);
  for (const exitCode of [124, 137]) {
    const terminated = sourceCalls().slice(0, 5);
    Object.assign(terminated[4]!, { exitCode, reply: sentinel });
    const result = await run(context, terminated);
    assert.equal(result.status, 1);
    assert.equal(result.events.at(-1).category, 'SOURCE_STAT_TIMEOUT');
    assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 0);
    assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
  }
});

test('real timed-out API subprocess is bounded, redacted and retried with a complete fresh collection', { timeout: 120_000 }, async context => {
  const result = await run(context, [
    { contains: ['get', 'clusters.postgresql.cnpg.io'], delayMs: 60_000, reply: sentinel },
    ...sourceCalls({ statDelayMs: 250 }),
  ]);
  assert.equal(result.status, 0);
  assert.equal(JSON.parse(result.stdout).sourceSpace.leaseSeconds, 45);
  const timeout = result.events.find(event => event.category === 'API_TIMEOUT' && event.event === 'api-end');
  assert.ok(timeout.durationMs >= 20_000 && timeout.durationMs < 30_000);
  assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 1);
  assert.equal(result.events.filter(event => event.event === 'collection-start').length, 2);
});

test('three transient subprocess failures never publish or leak stderr; malformed JSON is not retried', { timeout: 120_000 }, async context => {
  const failure: Step = { contains: ['get', 'clusters.postgresql.cnpg.io'], delayMs: 250, stderr: `Client.Timeout ${sentinel}`, reply: sentinel, exitCode: 1 };
  const result = await run(context, [failure, failure, failure]);
  assert.equal(result.status, 1);
  assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 2);
  assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
  const malformed = await run(context, [{ contains: ['get', 'clusters.postgresql.cnpg.io'], reply: sentinel }]);
  assert.equal(malformed.status, 1);
  assert.equal(malformed.events.at(-1).category, 'API_INVALID_JSON');
  assert.equal(malformed.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 0);
});

test('server timeout stderr is redacted and a complete fresh initial observation is retried', { timeout: 120_000 }, async context => {
  for (const kind of ['Timeout', 'ServerTimeout']) {
    const result = await run(context, [
      { contains: ['get', 'clusters.postgresql.cnpg.io'], stderr: `Error from server (${kind}): ${sentinel}`, exitCode: 1 },
      ...sourceCalls(),
    ]);
    assert.equal(result.status, 0);
    const failure = result.events.find(event => event.event === 'api-end' && event.outcome === 'failure');
    assert.equal(failure.category, 'API_TRANSIENT');
    assert.equal(failure.exitStatus, 1);
    assert.equal(failure.hasStderr, true);
    assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 1);
    assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
  }
});

const scheduleImage = 'ghcr.io/cloudnative-pg/postgresql@sha256:b1deeed2aa998b2f381e39c5cadb9ec06127708c8bd62965743af19abf21628f';
const docker = (args: string[], input?: string) => execFileSync('docker', args, { input, encoding: 'utf8', timeout: 180_000, stdio: ['pipe', 'pipe', 'pipe'] });
const shell = (container: string, command: string) => docker(['exec', container, '/bin/sh', '-ec', command]);
const quote = (value: string) => `'${value.replaceAll("'", "'\\''")}'`;

function supervisedStage(context: TestContext) {
  const stage = preparedResources().find(resource => resource.kind === 'Job' && resource.metadata.name === boundedJobs.stage).spec.template.spec.containers[0];
  const environment = Object.fromEntries(stage.env.filter((entry: any) => entry.value !== undefined).map((entry: any) => [entry.name, entry.value]));
  Object.assign(environment, {
    PARENT_REVIEW_APPROVED: 'true', SOURCE_IO_REVIEW_APPROVED: 'true', ROOT_STORAGE_REVIEW_APPROVED: 'true', HARD_CAPACITY_REVIEW_APPROVED: 'true',
    EXPECTED_BACKUP_FS_UUID: stageIdentity.filesystemUUID, EXPECTED_BACKUP_DEVICE: stageIdentity.device, POD_UID: exportPod.metadata.uid,
    EXPECTED_SERVER_ADDRESS: '127.0.0.1', DATABASE_URL: 'unused-fixture-no-credentials', PGOPTIONS: '',
    BACKUP_MAX_BYTES: String(128 * 1024 ** 2), BACKUP_MIN_BYTES: '1048576', BACKUP_RESERVE_BYTES: '0', CAPACITY_MIN_FREE_INODES: '1',
  });
  const container = docker(['run', '-d', '--network=none', '--read-only', '--user=26:26', '--cap-drop=ALL', '--security-opt=no-new-privileges',
    ...Object.entries(environment).flatMap(([name, value]) => ['--env', `${name}=${value}`]),
    '--tmpfs', '/tmp:rw,exec,size=32m,uid=26,gid=26', '--tmpfs', '/policy:rw,size=1m,uid=26,gid=26', '--tmpfs', '/backup:rw,size=128m,uid=26,gid=26',
    '--entrypoint=/bin/sleep', scheduleImage, '600']).trim();
  context.after(() => docker(['rm', '-f', container]));
  const write = (path: string, contents: string) => docker(['exec', '-i', container, 'dd', `of=${path}`, 'status=none'], contents);
  shell(container, 'mkdir /tmp/bin');
  write('/policy/capacity.sh', capacityPolicy);
  write('/tmp/bin/findmnt', `#!/bin/sh
case "$*" in
  *'/tmp') printf tmpfs ;;
  *FSTYPE*) printf xfs ;;
  *FSROOT*) printf / ;;
  *TARGET*) printf /backup ;;
  *UUID*) printf '' ;;
  *MAJ:MIN*) printf 8:32 ;;
  *SOURCE*) printf %s '${stageIdentity.device}' ;;
  *) exit 1 ;;
esac
`);
  write('/tmp/bin/psql', '#!/bin/sh\ncase "$*" in *"SELECT NOT pg_is_in_recovery()"*) printf "t\\n" ;; esac\n');
  write('/tmp/bin/pg_dump', '#!/bin/sh\ntrap "" TERM\nprintf "%s\\n" "$$" >> /tmp/producer-starts\ndd if=/dev/zero bs=65536 status=none &\nwait "$!"\n');
  const command = [...stage.command];
  command[6] = command[6].replace('26071793664', '1048576').replace('rate_pid=$!\n', 'rate_pid=$!\nprintf \'%s\\n\' "$rate_pid" >> /tmp/writer-starts\n');
  write('/tmp/export-command', `#!/bin/sh\nexport PATH=/tmp/bin:$PATH\nprintf '%s\\n' "$$" > /tmp/supervisor.pid\nexec ${command.map(quote).join(' ')}\n`);
  shell(container, 'chmod 700 /tmp/bin/* /tmp/export-command');
  const exporter = spawn('docker', ['exec', container, '/tmp/export-command'], { stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  exporter.stdout!.on('data', chunk => { output += chunk; });
  exporter.stderr!.on('data', chunk => { output += chunk; });
  context.after(() => { if (exporter.exitCode === null) exporter.kill('SIGKILL'); });
  return { container, exporter, output: () => output };
}

function assertSequentialCalls(calls: any[]): void {
  let active: number | undefined;
  for (const call of calls) {
    if (call.event === 'start') {
      assert.equal(active, undefined, 'No overlapping collector API processes');
      active = call.index;
    } else {
      assert.equal(call.index, active);
      active = undefined;
    }
  }
  assert.equal(active, undefined);
}

async function assertStoppedStage(stage: ReturnType<typeof supervisedStage>): Promise<void> {
  for (let attempt = 0; attempt < 100 && stage.exporter.exitCode === null; attempt++) await delay(100);
  assert.equal(stage.exporter.exitCode, 1, 'Independent supervisor must stop, never falsely succeed');
  const directory = '/backup/mtc-pg-logical-20261004';
  shell(stage.container, `test -s ${directory}/memeloop_token_center.dump.partial; test ! -e ${directory}/LOCAL_ARCHIVE_CREATED; test "$(wc -l < /tmp/producer-starts)" -eq 1; test "$(wc -l < /tmp/writer-starts)" -eq 1`);
  const remaining = docker(['top', stage.container, '-eo', 'pid,ppid,pgid,stat,comm']).trim().split('\n').slice(1).filter(line => !line.trim().split(/\s+/)[3]!.startsWith('Z'));
  assert.equal(remaining.length, 1, `Producer/writer descendants leaked: ${remaining.join('; ')}`);
  assert.equal(remaining[0]!.trim().split(/\s+/).at(-1), 'sleep', 'Unrelated PID1 remains alive');
  assert.match(stage.output(), /guard_abort=/);
  assert.doesNotMatch(stage.output(), /local archive SHA256 and TOC only/);
}

test('one 15s PVC context deadline recollects fully inside the unchanged 45s lease without restarting the export', { timeout: 120_000 }, async context => {
  docker(['pull', scheduleImage]);
  const stage = supervisedStage(context);
  const publish: Step = { contains: ['exec', exportName, '/bin/sh'], reply: '', leaseOperation: 'publish' };
  const revoke = revokeCalls();
  revoke[1]!.leaseOperation = 'revoke';
  const result = await run(context, [
    ...sourceCalls(), exportCall(), ...volumeCalls(), ...sourceCalls(), publish,
    exportCall(), { contains: ['get', 'pvc'], requestTimeout: '--request-timeout=15s', delayMs: 15_000, stderr: `Unable to connect to the server: context deadline exceeded ${sentinel}`, exitCode: 1 },
    exportCall(), ...volumeCalls(), ...sourceCalls(), publish,
    exportCall(), { contains: ['get', 'pvc'], reply: sentinel, stderr: sentinel, exitCode: 1 }, ...revoke,
  ], true, stage.container);
  assert.equal(result.status, 1);
  const timeout = result.events.find(event => event.event === 'api-end' && event.phase === 'volume-claim' && event.category === 'API_TIMEOUT');
  assert.ok(timeout.durationMs >= 15_000 && timeout.durationMs < 20_000);
  assert.equal(timeout.exitStatus, 1);
  assert.equal(timeout.hasStderr, true);
  assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 1);
  const leases = result.events.filter(event => event.event === 'lease-publication-end');
  assert.equal(leases.length, 2);
  assert.equal(leases[1].attempt, 2);
  assert.ok(leases.every(event => event.leaseSeconds === 45));
  assert.ok(Date.parse(leases[1].observedAt) / 1000 - leases[0].volumeLeaseEpoch < 45);
  assert.ok(Date.parse(leases[1].sourceSampleAt) > Date.parse(timeout.observedAt));
  const revocations = result.events.filter(event => event.phase === 'lease-revoke' && event.outcome === 'success');
  assert.equal(revocations.length, 1);
  assert.ok(Date.parse(revocations[0].observedAt) > Date.parse(leases[1].observedAt));
  assertSequentialCalls(result.calls);
  await assertStoppedStage(stage);
});

test('25s collection immediately starts the next 12s cycle; real 45s CSI TTL keeps one producer/writer alive until revocation', { timeout: 120_000 }, async context => {
  docker(['pull', scheduleImage]);
  const stage = supervisedStage(context);
  const slowVolume = volumeCalls();
  slowVolume[0]!.delayMs = 12500;
  slowVolume[1]!.delayMs = 12500;
  const nextSource = sourceCalls();
  nextSource[0]!.delayMs = 6000;
  nextSource[1]!.delayMs = 6000;
  const publish: Step = { contains: ['exec', exportName, '/bin/sh'], reply: '', leaseOperation: 'publish' };
  const revoke = revokeCalls();
  revoke[1]!.leaseOperation = 'revoke';
  const failedVolume = volumeCalls().slice(0, 1);
  Object.assign(failedVolume[0]!, { exitCode: 1, reply: sentinel, stderr: sentinel });
  const result = await run(context, [
    ...sourceCalls(), exportCall(), ...slowVolume, ...sourceCalls(), publish,
    exportCall(), ...volumeCalls(), ...nextSource, publish,
    exportCall(), ...failedVolume, ...revoke,
  ], true, stage.container);
  assert.equal(result.status, 1);
  const schedules = result.events.filter(event => event.event === 'renewal-schedule');
  assert.equal(schedules.length, 2);
  assert.ok(schedules[0].cycleElapsedMs >= 25000);
  assert.equal(schedules[0].delayMs, 0, 'Do not add a fixed 15s delay after slow collection');
  const firstEnd = result.events.find(event => event.event === 'collection-end' && event.cycle === 1);
  const secondStart = result.events.find(event => event.event === 'collection-start' && event.cycle === 2);
  assert.ok(secondStart.monotonicMs - firstEnd.monotonicMs < 2000, 'Overrun starts the next sequential cycle immediately');
  const leases = result.events.filter(event => event.event === 'lease-publication-end');
  assert.equal(leases.length, 2, 'Failed collection cannot publish a third lease');
  assert.ok(Date.parse(leases[0].observedAt) / 1000 - leases[0].volumeLeaseEpoch >= 25, 'Keep actual old observation epoch, do not retimestamp');
  assert.ok(Date.parse(leases[1].observedAt) / 1000 - leases[0].volumeLeaseEpoch < 45, 'Second publication arrives inside the original CSI TTL');
  assert.ok(result.events.some(event => event.phase === 'lease-revoke' && event.outcome === 'success'));
  assertSequentialCalls(result.calls);
  await assertStoppedStage(stage);
});

test('a collection exceeding 45s still expires the real volume lease and cleans up without renewal or overlapping writers', { timeout: 120_000 }, async context => {
  const stage = supervisedStage(context);
  const expiredVolume = volumeCalls();
  for (const step of expiredVolume.slice(0, 3)) step.delayMs = 16000;
  const revoke = revokeCalls();
  revoke[1]!.leaseOperation = 'revoke';
  const result = await run(context, [
    ...sourceCalls(), exportCall(), ...volumeCalls(), ...sourceCalls(),
    { contains: ['exec', exportName, '/bin/sh'], reply: '', leaseOperation: 'publish' },
    exportCall(), ...expiredVolume, ...revoke,
  ], true, stage.container);
  assert.equal(result.status, 1);
  assert.equal(result.events.filter(event => event.event === 'lease-publication-end').length, 1);
  assert.equal(result.events.filter(event => event.event === 'renewal-schedule').length, 1);
  assert.ok(result.events.some(event => event.event === 'collection-end' && event.cycle === 2 && event.durationMs >= 48000 && event.outcome === 'failure'));
  assert.equal(result.events.at(-1).category, 'GUARD_ASSERTION');
  assertSequentialCalls(result.calls);
  await assertStoppedStage(stage);
});
