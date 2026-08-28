import { spawnSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repository = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
for (const script of ['tests/ops/release-packaging-contract.test.ts', 'tests/ops/github-workflow-policy-fixtures.test.ts']) {
  const result = spawnSync(process.execPath, ['--test', script], { cwd: repository, encoding: 'utf8', stdio: 'inherit', shell: false });
  if (result.status !== 0) process.exit(result.status ?? 1);
}
