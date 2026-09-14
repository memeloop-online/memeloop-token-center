import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer, type InlineConfig } from 'vite';

/** Concurrent fixtures must not invalidate each other's optimized modules. */
export async function createIsolatedFixtureServer(config: InlineConfig) {
  const cacheDir = await mkdtemp(join(tmpdir(), 'mtc-vite-fixture-'));
  try {
    const server = await createServer({ ...config, cacheDir });
    const close = server.close.bind(server);
    server.close = async () => {
      try { await close(); }
      finally { await rm(cacheDir, { recursive: true, force: true }); }
    };
    return server;
  } catch (error) {
    await rm(cacheDir, { recursive: true, force: true });
    throw error;
  }
}
