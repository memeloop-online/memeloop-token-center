import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export function checkedCommandOutput(mode: string, value: unknown): string {
  assert(value && typeof value === 'object' && !Array.isArray(value), 'expected command output object');
  const output = value as Record<string, unknown>;
  if (mode === 'cosign-version') {
    assert.equal(output.gitVersion, 'v3.1.3-mtc.3', 'reviewed Cosign compatibility version required');
    return '';
  }
  assert.equal(mode, 'oras-push', 'unknown command output mode');
  assert(typeof output.digest === 'string' && /^sha256:[a-f0-9]{64}$/.test(output.digest), 'digest-pinned OCI push result required');
  return output.digest;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [mode = '', path] = process.argv.slice(2);
  const value = JSON.parse(readFileSync(path ?? 0, 'utf8'));
  process.stdout.write(checkedCommandOutput(mode, value));
}
