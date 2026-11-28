/** Loopback-only S3 fault transport. Signed path, query and end-to-end headers stay intact. */
import { createServer, request, type IncomingMessage } from 'node:http';
import { pipeline } from 'node:stream';
import { setTimeout as delay } from 'node:timers/promises';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

type Mode = 'normal' | 'slow' | 'fail';
const methods = new Set(['POST', 'PUT', 'GET', 'HEAD', 'DELETE']);
const hopHeaders = new Set(['connection', 'keep-alive', 'proxy-authenticate', 'proxy-authorization', 'te', 'trailer', 'transfer-encoding', 'upgrade']);

/** Preserve original header values, duplicate fields and signed Host; only remove hop-by-hop fields. */
function forwardedHeaders(message: IncomingMessage, response = false): string[] {
  const excluded = new Set(hopHeaders);
  for (const value of (message.headers.connection ?? '').split(',')) excluded.add(value.trim().toLowerCase());
  if (response) { excluded.add('server'); excluded.add('date'); }
  const result: string[] = [];
  for (let index = 0; index < message.rawHeaders.length; index += 2) {
    if (!excluded.has(message.rawHeaders[index].toLowerCase())) result.push(message.rawHeaders[index], message.rawHeaders[index + 1]);
  }
  return result;
}

export function createObjectProxy(options: { upstreamPort?: number; slowMillis?: number; onTransportError?: (kind: 'request' | 'response') => void } = {}) {
  const upstreamPort = options.upstreamPort ?? 19000;
  const slowMillis = options.slowMillis ?? 3000;
  let mode: Mode = 'normal';
  let faults = 0;
  const server = createServer((incoming, outgoing) => {
    const controller = new AbortController();
    let upstream: ReturnType<typeof request> | undefined;
    let reply: IncomingMessage | undefined;
    const cancel = () => { controller.abort(); upstream?.destroy(); reply?.destroy(); };
    incoming.once('aborted', cancel);
    outgoing.once('close', () => { if (!outgoing.writableFinished) cancel(); });
    const failTransport = (kind: 'request' | 'response') => {
      options.onTransportError?.(kind);
      upstream?.destroy(); reply?.destroy();
      if (outgoing.destroyed) return;
      if (outgoing.headersSent) outgoing.destroy();
      else { outgoing.writeHead(502); outgoing.end(); }
    };
    void (async () => {
      const method = incoming.method ?? '';
      const path = incoming.url ?? '/';
      if (!methods.has(method)) { incoming.resume(); outgoing.writeHead(501); outgoing.end(); return; }
      if (path.startsWith('/__test__/')) {
        const selected = path.slice(path.lastIndexOf('/') + 1);
        incoming.resume();
        if (selected === 'normal' || selected === 'slow' || selected === 'fail') {
          mode = selected; faults = 0; outgoing.writeHead(204); outgoing.end();
        } else { outgoing.writeHead(200); outgoing.end(String(faults)); }
        return;
      }
      const selected = mode;
      if (method === 'PUT' && selected !== 'normal') {
        faults += 1;
        if (selected === 'fail') { incoming.resume(); outgoing.writeHead(503); outgoing.end(); return; }
        try { await delay(slowMillis, undefined, { signal: controller.signal }); }
        catch { return; }
      }
      if (controller.signal.aborted) return;
      // Use node:http instead of fetch: no URL normalization, decompression,
      // redirects or body reserialization may alter the signed S3 exchange.
      upstream = request({ hostname: '127.0.0.1', port: upstreamPort, method, path,
        headers: forwardedHeaders(incoming), agent: false }, (response) => {
        reply = response;
        if (controller.signal.aborted) { response.destroy(); return; }
        outgoing.sendDate = false;
        outgoing.writeHead(response.statusCode ?? 502, forwardedHeaders(response, true));
        pipeline(response, outgoing, (error) => { if (error && !controller.signal.aborted) failTransport('response'); });
      });
      upstream.setTimeout(30_000, () => upstream?.destroy(new Error('upstream transport timeout')));
      upstream.once('error', () => { if (!controller.signal.aborted) failTransport('request'); });
      pipeline(incoming, upstream, (error) => { if (error && !controller.signal.aborted) failTransport('request'); });
    })().catch(() => failTransport('request'));
  });
  return server;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const server = createObjectProxy();
  server.listen(19001, '127.0.0.1');
  for (const signal of ['SIGTERM', 'SIGINT'] as const) process.once(signal, () => {
    server.closeAllConnections(); server.close();
  });
}
