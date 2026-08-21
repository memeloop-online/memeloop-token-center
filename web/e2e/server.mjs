import { createServer } from 'node:http';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(webRoot, '..');
const baseURL = new URL(process.env.MTC_E2E_BASE_URL ?? 'http://127.0.0.1:41739');
const listenHost = baseURL.hostname;
const listenPort = baseURL.port || '80';
const mockPort = Number(process.env.MTC_E2E_MOCK_PORT ?? 41740);
const testDirectory = mkdtempSync(join(tmpdir(), 'memeloop-token-center-e2e-'));
const pluginRoot = join(testDirectory, 'plugins');
const configurablePlugin = join(pluginRoot, 'browser-configuration');
mkdirSync(configurablePlugin, { recursive: true });
writeFileSync(join(configurablePlugin, 'plugin.json'), JSON.stringify({
  id: 'browser-configuration',
  version: '1.0.0',
  wit_version: '0.2.0',
  wasm: null,
  capabilities: [],
  contributions: {
    configuration: {
      schema: {
        type: 'object',
        additionalProperties: false,
        required: ['mode'],
        properties: { mode: { type: 'string', title: 'Mode', enum: ['default', 'configured'] } },
      },
      default: { mode: 'default' },
    },
    providers: [],
  },
}));
let application;
let stopping = false;
let stopPromise;
let comfySequence = 0;
let blockerActive = false;

const upstream = createServer((request, response) => {
  const chunks = [];
  request.on('data', (chunk) => chunks.push(chunk));
  request.on('end', () => {
    const requestUrl = new URL(request.url ?? '/', `http://127.0.0.1:${mockPort}`);
    if (request.method === 'GET' && requestUrl.pathname === '/v1/models') {
      response.writeHead(200, { 'content-type': 'application/json' });
      const filler = Array.from({ length: 205 }, (_, index) => ({ id: `mock-filler-model-${String(index).padStart(3, '0')}` }));
      response.end(JSON.stringify({ data: [{ id: 'mock-provider-model' }, ...filler, { id: 'mock-provider-model-v2' }] }));
      return;
    }
    if (request.method === 'GET' && requestUrl.pathname === '/__e2e/blocker-active') {
      response.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' });
      response.end(JSON.stringify({ active: blockerActive }));
      return;
    }
    if (request.method === 'POST' && requestUrl.pathname === '/api/v3/contents/generations/tasks') {
      let body;
      try { body = JSON.parse(Buffer.concat(chunks).toString('utf8')); }
      catch { body = undefined; }
      if (request.headers.authorization !== 'Bearer browser-seedance-secret-not-real'
        || body?.model !== 'seedance-browser-v1'
        || body?.duration !== 5
        || body?.content?.[0]?.text !== '一只狐狸跑过草地') {
        response.writeHead(400, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ error: { message: 'invalid browser Seedance request' } }));
        return;
      }
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ id: 'browser-seedance-video' }));
      return;
    }
    if (request.method === 'GET' && requestUrl.pathname === '/api/v3/contents/generations/tasks/browser-seedance-video') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({
        id: 'browser-seedance-video',
        status: 'succeeded',
        duration: '5',
        content: { video_url: `http://127.0.0.1:${mockPort}/assets/browser-video.mp4?provider_secret=never-persist` },
      }));
      return;
    }
    if (request.method === 'GET' && requestUrl.pathname === '/assets/browser-video.mp4') {
      response.writeHead(200, { 'content-type': 'video/mp4' });
      response.end(Buffer.from('browser-video-asset'));
      return;
    }
    if (request.method === 'POST' && requestUrl.pathname === '/prompt') {
      let body;
      try { body = JSON.parse(Buffer.concat(chunks).toString('utf8')); }
      catch { body = undefined; }
      const prompt = body?.prompt?.['9']?.inputs?.filename_prefix;
      if (typeof prompt !== 'string' || !prompt) {
        response.writeHead(400, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ error: { message: 'invalid browser ComfyUI workflow' } }));
        return;
      }
      if (prompt === 'browser-worker-blocker') {
        blockerActive = true;
        setTimeout(() => {
          blockerActive = false;
          response.writeHead(400, { 'content-type': 'application/json' });
          response.end(JSON.stringify({ error: { message: 'deterministic worker blocker released' } }));
        }, 5_000);
        return;
      }
      comfySequence += 1;
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ prompt_id: `browser-comfy-${comfySequence}` }));
      return;
    }
    if (request.method === 'GET' && requestUrl.pathname.startsWith('/history/browser-comfy-')) {
      const promptId = requestUrl.pathname.slice('/history/'.length);
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({
        [promptId]: {
          status: { status_str: 'success', completed: true },
          outputs: { '9': { images: [{ filename: 'browser-result.png', subfolder: '', type: 'output' }] } },
        },
      }));
      return;
    }
    if (request.method === 'GET' && requestUrl.pathname === '/view') {
      response.writeHead(200, { 'content-type': 'image/png' });
      response.end(Buffer.from('browser-png-asset'));
      return;
    }
    if (request.method !== 'POST' || requestUrl.pathname !== '/v1/chat/completions') {
      response.writeHead(404, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { message: 'mock route not found' } }));
      return;
    }
    let body;
    try {
      body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    } catch {
      response.writeHead(400, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { message: 'invalid mock request' } }));
      return;
    }
    const prompt = Array.isArray(body.messages)
      ? body.messages.map((message) => String(message?.content ?? '')).join('\n')
      : '';
    if (prompt.includes('force session active success') || prompt.includes('force session active error')) {
      const fail = prompt.includes('force session active error');
      setTimeout(() => {
        response.writeHead(fail ? 429 : 200, { 'content-type': 'application/json' });
        response.end(JSON.stringify(fail ? {
          error: { type: 'rate_limit_error', message: 'mock delayed session rate limit' },
        } : {
          id: 'chatcmpl-browser-session-live',
          object: 'chat.completion',
          created: Math.floor(Date.now() / 1000),
          model: body.model,
          choices: [{ index: 0, message: { role: 'assistant', content: 'browser delayed session response' }, finish_reason: 'stop' }],
          usage: { prompt_tokens: 8, completion_tokens: 4, total_tokens: 12 },
        }));
      }, 1_500);
      return;
    }
    if (prompt.includes('force observable error')) {
      response.writeHead(429, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { type: 'rate_limit_error', message: 'mock observable rate limit' } }));
      return;
    }
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({
      id: 'chatcmpl-browser-e2e',
      object: 'chat.completion',
      created: Math.floor(Date.now() / 1000),
      model: body.model,
      choices: [{ index: 0, message: { role: 'assistant', content: 'browser e2e response' }, finish_reason: 'stop' }],
      usage: { prompt_tokens: 8, completion_tokens: 4, total_tokens: 12 },
    }));
  });
});

function closeUpstream() {
  return new Promise((resolveClose) => upstream.close(() => resolveClose()));
}

function signalApplication(signal) {
  if (!application || application.exitCode !== null || application.signalCode !== null) return;
  try {
    if (process.platform === 'win32') application.kill(signal);
    else process.kill(-application.pid, signal);
  } catch (error) {
    if (error?.code !== 'ESRCH') throw error;
  }
}

function waitForApplication(timeoutMilliseconds) {
  if (!application || application.exitCode !== null || application.signalCode !== null) return Promise.resolve(true);
  return Promise.race([
    new Promise((resolveExit) => application.once('exit', () => resolveExit(true))),
    new Promise((resolveTimeout) => setTimeout(() => resolveTimeout(false), timeoutMilliseconds)),
  ]);
}

function stop(exitCode = 0) {
  if (stopPromise) return stopPromise;
  stopping = true;
  stopPromise = (async () => {
    const closingUpstream = closeUpstream();
    signalApplication('SIGTERM');
    if (!(await waitForApplication(10_000))) {
      signalApplication('SIGKILL');
      await waitForApplication(5_000);
    }
    await closingUpstream;
    rmSync(testDirectory, { recursive: true, force: true });
    process.exitCode = exitCode;
  })().catch((error) => {
    process.stderr.write(`browser e2e cleanup failed: ${error.message}\n`);
    process.exitCode = 1;
  });
  return stopPromise;
}

upstream.on('error', (error) => {
  process.stderr.write(`browser e2e mock upstream failed: ${error.message}\n`);
  stop(1);
});

upstream.listen(mockPort, '127.0.0.1', () => {
  process.send?.({ type: 'mock-listening' });
  const prebuiltBinary = process.env.MTC_E2E_BINARY;
  application = spawn(
    prebuiltBinary || 'cargo',
    prebuiltBinary
      ? ['serve', '--role', 'all']
      : ['run', '--quiet', '--manifest-path', join(repositoryRoot, 'Cargo.toml'), '--bin', 'memeloop-token-center', '--', 'serve', '--role', 'all'],
    {
      cwd: repositoryRoot,
      env: {
        ...process.env,
        MTC_LISTEN: `${listenHost}:${listenPort}`,
        MTC_DATABASE_URL: `sqlite://${join(testDirectory, 'browser.db')}?mode=rwc`,
        MTC_DATABASE_MAX_CONNECTIONS: '2',
        MTC_KEY_PEPPER: 'browser-e2e-pepper-is-not-a-real-secret-value',
        MTC_SERVICE_TOKEN: process.env.MTC_E2E_SERVICE_TOKEN ?? 'browser-e2e-bootstrap-not-a-real-token',
        MTC_ARCHIVE_BACKEND: 'filesystem',
        MTC_ARCHIVE_PATH: join(testDirectory, 'archive'),
        MTC_PLUGIN_DIR: pluginRoot,
        MTC_WEB_ROOT: join(webRoot, 'dist'),
        MTC_ALLOW_OAUTH_LOOPBACK: 'true',
        RUST_LOG: process.env.RUST_LOG ?? 'warn',
      },
      stdio: 'inherit',
      detached: process.platform !== 'win32',
    },
  );
  application.on('exit', (code, signal) => {
    if (!stopping) {
      process.stderr.write(`browser e2e application exited (${code ?? signal})\n`);
      stop(code ?? 1);
    }
  });
});

process.on('SIGINT', () => stop());
process.on('SIGTERM', () => stop());
process.on('exit', () => {
  if (!stopping) rmSync(testDirectory, { recursive: true, force: true });
});
