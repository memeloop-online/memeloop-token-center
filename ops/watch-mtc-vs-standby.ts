import { execFileSync } from 'node:child_process';
import { appendFileSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

type Resource = { metadata: { name: string; uid: string; resourceVersion: string; creationTimestamp: string; annotations?: Record<string, string>; ownerReferences?: { uid: string }[] }; spec: any; status?: any };
export type Config = { context: 'mtc-argocd-release'; receipt: string; attemptFile: string; mode: 'watch' | 'stop'; authorization: 'OWN_STANDBY_BOOTSTRAP_WATCH_STOP'; operation: 'vs-standby-20261012' };
export type Runtime = { command: (args: string[], input: string | undefined, timeout: number) => string; now: () => number; wall: () => number; pause: (milliseconds: number) => Promise<void>; read: (path: string) => any; write: (path: string, value: unknown) => void; record: (path: string, value: unknown) => void; emit: (message: string) => void; interrupted: () => boolean };
const namespace = 'memeloop-token-center';
const ownName = 'memeloop-token-center-pg-vs-standby';
const sourceName = 'memeloop-token-center-pg-7';
const sourceUID = 'cbcf3e22-0173-4898-a81a-6fb3a4b06266';
const originalClusterUID = '587427b2-ca1d-4497-8352-7e9897d205a7';
const application = 'mtc_vs_standby_20261012';
const gib = 1024 ** 3;
export async function runWatcher(config: Config, runtime: Runtime): Promise<number> {
if (config.context !== 'mtc-argocd-release' || config.authorization !== 'OWN_STANDBY_BOOTSTRAP_WATCH_STOP' || config.operation !== 'vs-standby-20261012' || !['watch', 'stop'].includes(config.mode) || !config.receipt.startsWith('/var/lib/mwc/build-scratch/tmp/mtc-20261008/') || !config.attemptFile.startsWith('/var/lib/mwc/build-scratch/tmp/mtc-20261008/') || config.attemptFile === config.receipt) throw Error('EXPLICIT_OWN_ONLY_AUTHORIZATION_REQUIRED');
const began = runtime.now();
let wallStarted = runtime.wall();
let cluster: Resource | undefined;
let baselineWal = 0;
let baselineFree = 0;
let baselineSlots = new Set<string>();
let stopReason = 'MONITOR_FAILURE';
let released = false;
let receiptFailed = false;
let readDeadline = Infinity;
let armed = false;
let baselineSenders = new Set<string>();
let exitCode = 1;
const children = new Map<string, { kind: string; name: string; uid: string }>();
const observedOwnPids = new Set<number>();
const observedOwnSlots = new Set<string>();
function record(event: string, detail: unknown = {}) {
  try { runtime.record(config.receipt, { time: new Date(runtime.wall()).toISOString(), event, detail }); }
  catch { receiptFailed = true; runtime.emit('PRIVATE_RECEIPT_FAILED'); }
}
function command(args: string[], input?: string) {
  const timeout = Math.floor(Math.min(5000, readDeadline - runtime.now()));
  if (timeout < 1) throw Error('MONITOR_INTERVAL_EXCEEDED');
  return runtime.command(['--context', config.context, '--request-timeout=4s', ...args], input, timeout);
}
function senderKey(sender: any) { return JSON.stringify([sender.pid, sender.started, sender.client, sender.port]); }
function persist() {
  runtime.write(config.attemptFile, { version: 1, operation: config.operation, sourceUID, originalClusterUID, wallStarted, armed, baselineWal, baselineFree, baselineSlots: [...baselineSlots], baselineSenders: [...baselineSenders], cluster, children: [...children.values()], observedOwnPids: [...observedOwnPids], observedOwnSlots: [...observedOwnSlots] });
}
function restore() {
  const saved = runtime.read(config.attemptFile);
  if (saved.version !== 1 || saved.operation !== config.operation || saved.sourceUID !== sourceUID || saved.originalClusterUID !== originalClusterUID || saved.armed !== true || !Number.isFinite(saved.wallStarted) || !Array.isArray(saved.baselineSlots) || !Array.isArray(saved.baselineSenders)) throw Error('TRUSTED_ATTEMPT_HANDLE_REQUIRED');
  wallStarted = saved.wallStarted;
  armed = true;
  baselineWal = saved.baselineWal;
  baselineFree = saved.baselineFree;
  baselineSlots = new Set(saved.baselineSlots);
  baselineSenders = new Set(saved.baselineSenders);
  if (saved.cluster) bindOwn(saved.cluster);
  for (const child of saved.children ?? []) {
    if (!['pods', 'jobs'].includes(child.kind) || !child.uid || !child.name.startsWith(ownName + '-')) throw Error('INVALID_OWN_CHILD_HANDLE');
    children.set(child.uid, child);
  }
  for (const pid of saved.observedOwnPids ?? []) observedOwnPids.add(pid);
  for (const slot of saved.observedOwnSlots ?? []) observedOwnSlots.add(slot);
}
function get(kind: string, name: string): Resource {
  return JSON.parse(command(['-n', namespace, 'get', kind, name, '-o', 'json']));
}
function sourceIdentity() {
  const pod = get('pod', sourceName);
  const original = get('cluster', 'memeloop-token-center-pg');
  if (pod.metadata.uid !== sourceUID || original.metadata.uid !== originalClusterUID || original.status.currentPrimary !== sourceName || !pod.status.conditions.some((condition: any) => condition.type === 'Ready' && condition.status === 'True')) throw Error('SOURCE_IDENTITY_OR_HEALTH_CHANGED');
}
function sample() {
  const sql = `BEGIN READ ONLY; SET LOCAL ROLE pg_monitor; SET LOCAL statement_timeout='3s'; SET LOCAL lock_timeout='500ms';
SELECT json_build_object('identity',current_user,'readOnly',current_setting('transaction_read_only'),'superuser',(SELECT rolsuper FROM pg_roles WHERE rolname=current_user),'recovery',pg_is_in_recovery(),
'wal',(SELECT coalesce(sum(size),0) FROM pg_ls_waldir()),
'slots',(SELECT coalesce(json_agg(json_build_object('name',slot_name,'temporary',temporary,'activePid',active_pid,'retained',pg_wal_lsn_diff(pg_current_wal_lsn(),restart_lsn))),'[]'::json) FROM pg_replication_slots),
'senders',(SELECT coalesce(json_agg(json_build_object('pid',pid,'started',backend_start,'application',application_name,'client',client_addr,'port',client_port)),'[]'::json) FROM pg_stat_replication)); COMMIT;`;
  const output = command(['-n', namespace, 'exec', '-i', sourceName, '-c', 'postgres', '--', 'env', 'PGOPTIONS=-c default_transaction_read_only=on -c statement_timeout=3000', 'psql', '-X', '-q', '-A', '-t', '-v', 'ON_ERROR_STOP=1', '-U', 'postgres', '-d', 'postgres'], sql);
  const result = JSON.parse(output.trim());
  if (result.identity !== 'pg_monitor' || result.readOnly !== 'on' || result.superuser !== false || result.recovery) throw Error('READONLY_SOURCE_NOT_PROVEN');
  const df = command(['-n', namespace, 'exec', sourceName, '-c', 'postgres', '--', 'df', '-Pk', '/var/lib/postgresql/data/pgdata']).trim().split('\n').at(-1)!.trim().split(/\s+/);
  result.free = Number(df.at(-3)) * 1024;
  if (!Number.isFinite(result.free) || !Number.isFinite(result.wal)) throw Error('INVALID_SOURCE_MEASUREMENTS');
  return result;
}
function bindOwn(candidate: Resource) {
  const spec = candidate.spec;
  if (candidate.metadata.name !== ownName || candidate.metadata.annotations?.['recovery.mtc/operation'] !== config.operation || Date.parse(candidate.metadata.creationTimestamp) < wallStarted || spec.instances !== 1 || spec.replica?.enabled !== true || spec.replica.source !== 'pg7-canonical' || spec.storage.size !== '40Gi' || spec.storage.storageClass !== 'mtc-cnpg-archive-versetensor-20261005' || spec.affinity.nodeSelector['kubernetes.io/hostname'] !== 'versetensor-hv' || spec.externalClusters?.length !== 1 || spec.externalClusters[0].connectionParameters.application_name !== application || spec.resources.requests.cpu !== '1' || spec.resources.requests.memory !== '2Gi' || spec.resources.limits.cpu !== '1' || spec.resources.limits.memory !== '2Gi') throw Error('OWN_CLUSTER_IDENTITY_OR_SPEC_MISMATCH');
  if (cluster && cluster.metadata.uid !== candidate.metadata.uid) throw Error('OWN_CLUSTER_REPLACED');
  cluster = candidate;
}
function discoverChildren() {
  if (!cluster) return;
  for (const kind of ['jobs', 'pods']) {
    const resources = JSON.parse(command(['-n', namespace, 'get', kind, '-l', `cnpg.io/cluster=${ownName}`, '-o', 'json'])).items as Resource[];
    for (const resource of resources) {
      const owned = resource.metadata.ownerReferences?.some(owner => owner.uid === cluster!.metadata.uid || children.has(owner.uid));
      if (!owned) throw Error('OWN_LABEL_WITHOUT_OWNER_UID');
      if (kind === 'pods' && resource.spec.nodeName && resource.spec.nodeName !== 'versetensor-hv') throw Error('OWN_POD_WRONG_NODE');
      children.set(resource.metadata.uid, { kind, name: resource.metadata.name, uid: resource.metadata.uid });
    }
  }
}
function bindSourceConnections(senders: any[]) {
  const sockets: { port: number; podUID: string; podIP: string }[] = [];
  for (const child of children.values()) {
    if (child.kind !== 'pods') continue;
    const pod = get('pod', child.name);
    if (pod.metadata.uid !== child.uid) throw Error('OWN_POD_UID_CHANGED');
    const running = pod.status?.containerStatuses?.filter((container: any) => container.state?.running) ?? [];
    if (!running.length) continue;
    const container = running.find((status: any) => ['postgres', 'join', 'pgbasebackup'].includes(status.name));
    if (!container) throw Error('OWN_RUNNING_CONTAINER_UNRECOGNIZED');
    const table = command(['-n', namespace, 'exec', child.name, '-c', container.name, '--', 'cat', '/proc/net/tcp']);
    for (const line of table.trim().split('\n').slice(1)) {
      const fields = line.trim().split(/\s+/);
      const local = fields[1];
      const remote = fields[2];
      if (!local || !remote) throw Error('OWN_SOCKET_METADATA_MALFORMED');
      const [remoteAddress, remotePort] = remote.split(':');
      const localPort = local.split(':')[1];
      if (!remoteAddress || !remotePort || !localPort) throw Error('OWN_SOCKET_METADATA_MALFORMED');
      if (fields[3] !== '01' || remotePort !== '1538' || !['19042A0A', 'CB432B0A'].includes(remoteAddress)) continue;
      sockets.push({ port: parseInt(localPort, 16), podUID: child.uid, podIP: pod.status.podIP });
    }
  }
  for (const sender of senders.filter(sender => sender.application === application)) {
    const matches = sockets.filter(socket => socket.port === sender.port && [socket.podIP, '100.64.0.8'].includes(sender.client));
    const match = matches[0];
    if (matches.length !== 1 || !match) throw Error('OWN_SOURCE_SOCKET_IDENTITY_UNPROVEN');
    observedOwnPids.add(sender.pid);
    record('OWN_SOURCE_SOCKET_BOUND', { sourcePid: sender.pid, client: sender.client, port: sender.port, podUID: match.podUID });
  }
}
function deleteUID(kind: string, name: string, uid: string) {
  const group = kind === 'clusters' ? '/apis/postgresql.cnpg.io/v1' : kind === 'jobs' ? '/apis/batch/v1' : '/api/v1';
  command(['delete', `--raw=${group}/namespaces/${namespace}/${kind}/${name}`, '-f', '-'], JSON.stringify({ apiVersion: 'v1', kind: 'DeleteOptions', propagationPolicy: 'Foreground', gracePeriodSeconds: 5, preconditions: { uid } }));
  record('OWN_UID_DELETE_ACK', { kind, name, uid });
}
async function stopOwn() {
  readDeadline = runtime.now() + 30000;
  let confirmed = false;
  record('STOP', { reason: stopReason, ownUID: cluster?.metadata.uid });
  if (!armed) { record(config.mode === 'stop' ? 'STOP_UNCONFIRMED_OPERATOR_REQUIRED' : 'PREFLIGHT_FAILED_NOT_ARMED'); exitCode = config.mode === 'stop' ? 2 : 1; return; }
  try {
    const raw = command(['-n', namespace, 'get', 'cluster', ownName, '--ignore-not-found', '-o', 'json']).trim();
    if (raw) { bindOwn(JSON.parse(raw)); persist(); discoverChildren(); persist(); }
  } catch {
    record('OWN_HANDLE_RECOVERY_FAILED');
    if (!cluster) { record('STOP_UNCONFIRMED_OPERATOR_REQUIRED'); exitCode = 2; return; }
  }
  let clusterDeleteAccepted = false;
  try { if (cluster) { deleteUID('clusters', ownName, cluster.metadata.uid); clusterDeleteAccepted = true; } }
  catch { record('OWN_CLUSTER_DELETE_ACK_UNKNOWN_NO_RETRY'); }
  for (const resource of [...children.values()].sort((left, right) => left.kind === right.kind ? 0 : left.kind === 'jobs' ? -1 : 1)) {
    try { deleteUID(resource.kind, resource.name, resource.uid); }
    catch { record('OWN_CHILD_DELETE_ACK_UNKNOWN_NO_RETRY', resource); }
  }
  while (runtime.now() < readDeadline) {
    try {
      const own = command(['-n', namespace, 'get', 'cluster', ownName, '--ignore-not-found', '-o', 'json']).trim();
      sourceIdentity();
      const remaining = sample();
      sourceIdentity();
      const sourceConnectionsGone = remaining.senders.every((sender: any) => baselineSenders.has(senderKey(sender)) && sender.application !== application && !observedOwnPids.has(sender.pid));
      const tempSlotsGone = remaining.slots.every((slot: any) => baselineSlots.has(slot.name) && !observedOwnSlots.has(slot.name));
      const ownChildrenGone = JSON.parse(command(['-n', namespace, 'get', 'pods,jobs', '-l', `cnpg.io/cluster=${ownName}`, '-o', 'json'])).items.length === 0;
      let knownUIDsGone = true;
      for (const child of children.values()) {
        const raw = command(['-n', namespace, 'get', child.kind, child.name, '--ignore-not-found', '-o', 'json']).trim();
        if (raw && JSON.parse(raw).metadata.uid === child.uid) knownUIDsGone = false;
      }
      const app = JSON.parse(command(['-n', 'argocd', 'get', 'application', ownName, '-o', 'json']));
      if (!own && ownChildrenGone && knownUIDsGone && sourceConnectionsGone && tempSlotsGone && !app.operation && !app.spec.syncPolicy?.automated && !receiptFailed) { confirmed = true; break; }
    } catch { record('STOP_VERIFICATION_READ_FAILED'); break; }
    if (runtime.now() + 1000 >= readDeadline) break;
    await runtime.pause(1000);
  }
  record(confirmed ? 'OWN_STOP_CONFIRMED' : 'STOP_UNCONFIRMED_OPERATOR_REQUIRED', { clusterDeleteAccepted, originalClusterPreserved: true, existingClaimsUntouched: true, retainPV: true, sourceBackendKill: false, sourceSlotDrop: false });
  exitCode = confirmed ? 3 : 2;
}
try {
  if (config.mode === 'stop') { restore(); stopReason = 'RECOVERED_OWN_STOP'; }
  else {
  readDeadline = runtime.now() + 15000;
  sourceIdentity();
  const app = JSON.parse(command(['-n', 'argocd', 'get', 'application', ownName, '-o', 'json']));
  if (app.operation || app.spec.syncPolicy?.automated) throw Error('OWN_APP_NOT_MANUAL_AND_IDLE');
  if (command(['-n', namespace, 'get', 'cluster', ownName, '--ignore-not-found', '-o', 'json']).trim()) throw Error('EXISTING_OWN_NAME_NOT_NEW_ATTEMPT');
  const initial = sample();
  if (initial.free < 12 * gib || initial.senders.length > 6 || initial.slots.length > 30) throw Error('SOURCE_PREFLIGHT_HEADROOM_FAILED');
  baselineWal = initial.wal;
  baselineFree = initial.free;
  baselineSlots = new Set(initial.slots.map((slot: any) => slot.name));
  baselineSenders = new Set(initial.senders.map(senderKey));
  armed = true;
  persist();
  record('ARMED_BEFORE_EXTERNAL_EXACT_RESOURCE_SYNC', initial);
  if (receiptFailed) throw Error('PRIVATE_RECEIPT_FAILED');
  runtime.emit('ARMED: external approved exact-resource GitOps sync only; this executor creates nothing');
  while (runtime.now() - began < 20 * 60 * 1000) {
    const tick = runtime.now();
    readDeadline = tick + 15000;
    if (runtime.interrupted()) { stopReason = 'OPERATOR_SIGNAL_STOP'; break; }
    const raw = command(['-n', namespace, 'get', 'cluster', ownName, '--ignore-not-found', '-o', 'json']).trim();
    if (raw) { bindOwn(JSON.parse(raw)); persist(); discoverChildren(); persist(); }
    sourceIdentity();
    const current = sample();
    bindSourceConnections(current.senders);
    const ownSlots = current.slots.filter((slot: any) => observedOwnPids.has(slot.activePid));
    for (const slot of ownSlots) observedOwnSlots.add(slot.name);
    persist();
    const unexplainedSlots = current.slots.filter((slot: any) => !baselineSlots.has(slot.name) && !ownSlots.includes(slot));
    record('SAMPLE', { ...current, ownSlots, unexplainedSlots, ownUID: cluster?.metadata.uid, sourceIdentityIncludesOwnPodSocket: true });
    if (receiptFailed) { stopReason = 'PRIVATE_RECEIPT_FAILED'; break; }
    if (current.free <= 8 * gib) { stopReason = 'SOURCE_FREE_LE_8GIB'; break; }
    if (ownSlots.some((slot: any) => Number(slot.retained) >= 2 * gib)) { stopReason = 'OWN_RETAINED_GE_2GIB'; break; }
    if (unexplainedSlots.length) { stopReason = 'NEW_SLOT_OWNERSHIP_UNPROVEN'; break; }
    if (current.wal - baselineWal >= 4 * gib) { stopReason = 'SOURCE_WAL_DIRECTORY_GROWTH_GE_4GIB'; break; }
    if (baselineFree - current.free >= 4 * gib) { stopReason = 'SOURCE_FILESYSTEM_USED_GROWTH_GE_4GIB'; break; }
    if (cluster?.status?.readyInstances === 1 && cluster.status.phase === 'Cluster in healthy state') { released = true; exitCode = 4; record('READY_BOOTSTRAP_ONLY_SUPERVISOR_HANDOFF_REQUIRED', { uid: cluster.metadata.uid }); break; }
    const duration = runtime.now() - tick;
    if (duration >= 15000) throw Error('MONITOR_INTERVAL_EXCEEDED');
    await runtime.pause(Math.max(0, 10000 - duration));
  }
  if (!released && stopReason === 'MONITOR_FAILURE') stopReason = 'BOOTSTRAP_20MIN_DEADLINE';
  }
} catch (error) {
  stopReason = error instanceof Error && /^[A-Z0-9_]+$/.test(error.message) ? error.message : 'MONITOR_FAILURE';
  record('MONITOR_FAILED', { reason: stopReason });
} finally {
  if (!released) await stopOwn();
  record('EXIT', { released, exitCode, elapsedMs: runtime.now() - began, executorCreatesResources: false });
}
return exitCode;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  let interrupted = false;
  process.on('SIGINT', () => { interrupted = true; });
  process.on('SIGTERM', () => { interrupted = true; });
  const runtime: Runtime = {
    command: (args, input, timeout) => execFileSync('kubectl', args, { encoding: 'utf8', input, timeout, maxBuffer: 1024 * 1024, stdio: ['pipe', 'pipe', 'pipe'] }),
    now: () => performance.now(), wall: () => Date.now(),
    pause: milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds)),
    read: path => JSON.parse(readFileSync(path, 'utf8')),
    write: (path, value) => { const temporary = `${path}.${process.pid}.tmp`; writeFileSync(temporary, JSON.stringify(value) + '\n', { mode: 0o600, flag: 'wx' }); renameSync(temporary, path); },
    record: (path, value) => appendFileSync(path, JSON.stringify(value) + '\n', { mode: 0o600 }),
    emit: message => console.log(message), interrupted: () => interrupted,
  };
  try {
    const configPath = process.argv[2];
    if (!configPath) throw Error('CONFIG_PATH_REQUIRED');
    process.exitCode = await runWatcher(JSON.parse(readFileSync(configPath, 'utf8')), runtime);
  }
  catch { console.error('INVALID_CONFIG_OR_UNCONFIRMED_STOP'); process.exitCode = 2; }
}
