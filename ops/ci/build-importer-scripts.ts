import { cpSync, mkdirSync, rmSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const repository = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const output = resolve(repository, 'dist/operator-scripts');
rmSync(output, { recursive: true, force: true });
await build({
  absWorkingDir: repository,
  entryPoints: [
    'ops/migrate-cpamp.ts',
    'ops/audit-cpa-migration.ts',
    'ops/import-cpa-session-archive.ts',
    'ops/legacy-credentials/attach-legacy-cpa-credentials.ts',
    'ops/cpa-upstreams/import-cpa-upstreams.ts',
    'ops/cpa-upstreams/generate-source-identity-key.ts',
    'ops/export-cpa-session-archive-delta.ts',
  ],
  bundle: true,
  platform: 'node',
  target: 'node24.18',
  format: 'esm',
  outExtension: { '.js': '.mjs' },
  outdir: output,
  logLevel: 'info',
});
mkdirSync(resolve(output, 'sql/cpamp'), { recursive: true });
cpSync(resolve(repository, 'ops/sql/cpamp'), resolve(output, 'sql/cpamp'), { recursive: true, force: false });
