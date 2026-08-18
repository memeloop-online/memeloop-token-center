import { createServer } from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
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
let application;
let stopping = false;
let stopPromise;

const upstream = createServer((request, response) => {
  const chunks = [];
  request.on('data', (chunk) => chunks.push(chunk));
  request.on('end', () => {
    if (request.method === 'GET' && request.url === '/v1/models') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ data: [{ id: 'mock-provider-model' }] }));
      return;
    }
    if (request.method !== 'POST' || request.url !== '/v1/chat/completions') {
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
  application = spawn(
    'cargo',
    ['run', '--quiet', '--manifest-path', join(repositoryRoot, 'Cargo.toml'), '--bin', 'memeloop-token-center', '--', 'serve', '--role', 'all'],
    {
      cwd: repositoryRoot,
      env: {
        ...process.env,
        MTC_LISTEN: `${listenHost}:${listenPort}`,
        MTC_DATABASE_URL: `sqlite://${join(testDirectory, 'browser.db')}?mode=rwc`,
        MTC_DATABASE_MAX_CONNECTIONS: '2',
        MTC_KEY_PEPPER: 'browser-e2e-pepper-is-not-a-real-secret-value',
        MTC_SERVICE_TOKEN: process.env.MTC_E2E_SERVICE_TOKEN ?? 'browser-e2e-bootstrap-not-a-real-token',
        MTC_ARCHIVE_BACKEND: 'memory',
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
