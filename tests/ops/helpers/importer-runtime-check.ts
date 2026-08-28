#!/usr/bin/env node
import { accessSync, constants, existsSync, statSync } from 'node:fs';
import { spawnSync } from 'node:child_process';

if (process.getuid?.() !== 10001 || process.getgid?.() !== 10001) process.exit(2);
for (const name of [
  'migrate-cpamp', 'audit-cpa-migration', 'import-cpa-session-archive-wrapper',
  'attach-legacy-cpa-credentials', 'import-cpa-upstreams',
  'generate-source-identity-key', 'export-cpa-session-archive-delta',
]) {
  const path = `/usr/local/bin/${name}`;
  accessSync(path, constants.X_OK);
  if ((statSync(path).mode & 0o777) !== 0o555) process.exit(3);
  try { accessSync(path, constants.W_OK); process.exit(4); } catch { /* expected */ }
}
for (const tool of ['psql', 'node', 'sqlite3', 'flock']) {
  if ((spawnSync(tool, ['--version'], { stdio: 'ignore', shell: false }).error as NodeJS.ErrnoException | undefined)?.code === 'ENOENT') process.exit(5);
}
if (process.versions.node.split('.')[0] !== '24') process.exit(6);
for (const path of ['/tests', '/source', '/work']) if (existsSync(path)) process.exit(7);
