import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { run } from './contract-helpers.ts';

function scopes(event: 'pull_request' | 'push', paths: string[]): Record<string, string> {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-ci-scope-contract-'));
  try {
    const changed = join(temporary, 'changed.txt');
    const output = join(temporary, 'output.txt');
    writeFileSync(changed, paths.length === 0 ? '' : `${paths.join('\n')}\n`);
    writeFileSync(output, '');
    run(process.execPath, ['ops/ci/detect-expensive-ci-scopes.ts', event, changed, output]);
    return Object.fromEntries(readFileSync(output, 'utf8').trim().split('\n').map((line) => line.split('=', 2)));
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

test('memory acceptance scope skips only reviewed non-binary paths', () => {
  assert.deepEqual(
    scopes('pull_request', ['docs/performance.md', 'web/src/App.tsx', 'charts/memeloop-token-center/values.yaml']),
    { memory: 'false', plugin_installer: 'false' },
  );
  for (const path of [
    'src/main.rs',
    'Cargo.toml',
    'Cargo.lock',
    'build.rs',
    'Dockerfile',
    '.cargo/config.toml',
    'migrations/common/0073_future.sql',
    'schemas/core-config.schema.json',
    'wit/policy.wit',
    'vendor/rust_decimal/src/lib.rs',
    'future-runtime-input.txt',
  ]) {
    assert.equal(scopes('pull_request', [path]).memory, 'true', `${path} must run memory acceptance`);
  }
});

test('master push remains unconditionally full and plugin inputs remain covered', () => {
  assert.deepEqual(scopes('push', []), { memory: 'true', plugin_installer: 'true' });
  for (const path of [
    '.dockerignore',
    'Dockerfile.plugin-installer',
    'Cargo.lock',
    'src/bin/install-plugin-oci.rs',
    'packaging/cosign/v3.1.3-security.patch',
  ]) {
    assert.equal(scopes('pull_request', [path]).plugin_installer, 'true', `${path} must build the plugin installer`);
  }
});
