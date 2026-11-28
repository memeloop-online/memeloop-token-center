/** Run-specific Service creation and UID-fenced deletion. No cluster calls on import. */
import { execFileSync, spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const namespace = 'memeloop-token-center-staging';
const requireCondition = (ok: unknown, message: string): void => { if (!ok) throw new Error(message); };
export type Metadata = { context: string; clusterUid: string; namespaceUid: string };
type Proxy = { url: string; stop: () => Promise<void> };
type Dependencies = {
  kubectl: (args: string[], input?: string) => any;
  proxy: (context: string) => Promise<Proxy>;
  remove: (url: string, body: string) => Promise<number>;
};

export async function startProxy(context: string): Promise<Proxy> {
  // kubectl owns the ephemeral port continuously; never probe then release a port.
  const child = spawn('kubectl', ['--context', context, 'proxy', '--address=127.0.0.1', '--port=0'], { stdio: ['ignore', 'pipe', 'ignore'] });
  let failedToStart = false;
  child.once('error', () => { failedToStart = true; });
  const stop = async (): Promise<void> => {
    if (failedToStart) return;
    if (child.exitCode !== null || child.signalCode !== null) return;
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => child.kill('SIGKILL'), 5000);
      child.once('exit', () => { clearTimeout(timer); resolve(); });
      child.kill('SIGTERM');
    });
  };
  try {
    const url = await new Promise<string>((resolve, reject) => {
      let output = '';
      const timer = setTimeout(() => reject(new Error('proxy readiness timeout')), 5000);
      const fail = (): void => { clearTimeout(timer); reject(new Error('proxy exited before readiness')); };
      child.once('error', fail);
      child.once('exit', fail);
      child.stdout!.on('data', (chunk) => {
        output = (output + String(chunk)).slice(-4096);
        const match = /Starting to serve on 127\.0\.0\.1:(\d+)/u.exec(output);
        if (match) { clearTimeout(timer); resolve(`http://127.0.0.1:${match[1]}`); }
      });
    });
    requireCondition(child.exitCode === null && child.signalCode === null, 'proxy exited before delete');
    return { url, stop };
  } catch (error) { await stop(); throw error; }
}

const defaults: Dependencies = {
  kubectl: (args, input) => JSON.parse(execFileSync('kubectl', args, { encoding: 'utf8', input, timeout: 15000 })),
  proxy: startProxy,
  remove: async (url, body) => (await fetch(url, { method: 'DELETE', headers: { 'Content-Type': 'application/json' }, body, signal: AbortSignal.timeout(5000), redirect: 'error' })).status,
};

export async function serviceAction(action: string, meta: Metadata, run: string, uid?: string, deps: Dependencies = defaults): Promise<object> {
  requireCondition(/^[a-z0-9-]{1,40}$/u.test(run) && ['create', 'delete'].includes(action), 'invalid action or run');
  requireCondition(typeof meta.context === 'string' && meta.context.length > 0 && typeof meta.clusterUid === 'string' && typeof meta.namespaceUid === 'string', 'invalid metadata');
  if (action === 'delete') requireCondition(typeof uid === 'string' && /^[a-f0-9-]{36}$/u.test(uid), 'invalid UID');
  const kubectl = (...args: string[]) => deps.kubectl(['--context', meta.context, ...args, '-o', 'json']);
  requireCondition(kubectl('get', 'namespace', 'kube-system').metadata.uid === meta.clusterUid, 'cluster UID mismatch');
  requireCondition(kubectl('get', 'namespace', namespace).metadata.uid === meta.namespaceUid, 'namespace UID mismatch');
  const name = `mtc-peak-${run}-mock`;
  if (action === 'create') {
    const service = { apiVersion: 'v1', kind: 'Service', metadata: { name, namespace, labels: { 'app.kubernetes.io/part-of': 'mtc-staging-peak', 'mtc.memeloop.ai/peak-run': run } }, spec: { selector: { 'app.kubernetes.io/part-of': 'mtc-staging-peak', 'mtc.memeloop.ai/peak-run': run, 'app.kubernetes.io/component': 'mock' }, ports: [{ port: 18080, targetPort: 8080 }] } };
    const created = deps.kubectl(['--context', meta.context, '-n', namespace, 'create', '-f', '-', '-o', 'json'], JSON.stringify(service));
    return { name, uid: created.metadata.uid, namespaceUid: meta.namespaceUid, runId: run };
  }
  const current = kubectl('-n', namespace, 'get', 'service', name);
  requireCondition(current.metadata.uid === uid && current.metadata.labels?.['mtc.memeloop.ai/peak-run'] === run, 'service identity mismatch');
  const proxy = await deps.proxy(meta.context);
  try {
    // Once sent, any missing or rejected response is unconfirmed: never retry DELETE.
    const status = await deps.remove(`${proxy.url}/api/v1/namespaces/${namespace}/services/${name}`, JSON.stringify({ apiVersion: 'v1', kind: 'DeleteOptions', preconditions: { uid } }));
    requireCondition(status === 200 || status === 202, `delete unconfirmed: HTTP ${status}`);
    return { name, deletedUid: uid };
  } catch (error) { throw new Error('delete unconfirmed; reconcile Service UID before another operation', { cause: error }); }
  finally { await proxy.stop(); }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [action, path, run, uid] = process.argv.slice(2);
  try {
    if (!action || !path || !run) throw new Error('usage: create|delete metadata.json run [uid]');
    console.log(JSON.stringify(await serviceAction(action, JSON.parse(readFileSync(path, 'utf8')), run, uid)));
  }
  catch (error) { console.error(error instanceof Error ? error.message : 'service operation failed'); process.exitCode = 1; }
}
