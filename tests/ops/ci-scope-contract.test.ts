import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { rejected, run } from './contract-helpers.ts';

type Change = readonly [status: string, ...paths: string[]];

function encoded(changes: readonly Change[]): string {
  return changes.flatMap(([status, ...paths]) => [status, ...paths]).join('\0') + (changes.length === 0 ? '' : '\0');
}

function scopes(event: 'pull_request' | 'push', changes: readonly Change[]): Record<string, string> {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-ci-scope-contract-'));
  try {
    const changed = join(temporary, 'changed.z');
    const output = join(temporary, 'output.txt');
    writeFileSync(changed, encoded(changes));
    writeFileSync(output, '');
    run(process.execPath, ['ops/ci/detect-expensive-ci-scopes.ts', event, changed, output]);
    return Object.fromEntries(readFileSync(output, 'utf8').trim().split('\n').map((line) => line.split('=', 2)));
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

const webOnly = { rust: 'false', migration: 'false', memory: 'false', plugin_installer: 'false' };
const full = { rust: 'true', migration: 'true', memory: 'true', plugin_installer: 'false' };
const fullPlugin = { ...full, plugin_installer: 'true' };

test('scope matrix permits only web-only pull requests to skip Rust and migration gates', () => {
  const matrix: readonly [label: string, changes: readonly Change[], expected: Record<string, string>][] = [
    ['PR 66-shaped web source and browser contract', [['M', 'web/src/useAnchoredPopover.ts'], ['A', 'web/e2e/popover-width-browser-contract.test.ts']], webOnly],
    ['web deletion', [['D', 'web/e2e/obsolete-browser-contract.test.ts']], webOnly],
    ['web-only rename', [['R100', 'web/src/old.tsx', 'web/src/new.tsx']], webOnly],
    ['production renamed into web', [['R100', 'src/api/routes.rs', 'web/src/routes.ts']], fullPlugin],
    ['web renamed into production', [['R100', 'web/src/routes.ts', 'src/api/routes.rs']], fullPlugin],
    ['shared manifest', [['M', 'package-lock.json']], full],
    ['Rust manifest', [['M', 'Cargo.lock']], fullPlugin],
    ['migration', [['D', 'migrations/0099_retired.sql']], fullPlugin],
    ['CI workflow', [['M', '.github/workflows/ci.yml']], fullPlugin],
    ['unknown path', [['A', 'future-runtime-input.txt']], full],
  ];
  for (const [label, changes, expected] of matrix) assert.deepEqual(scopes('pull_request', changes), expected, label);
});

test('known documentation and ordinary test paths preserve existing memory policy but not Rust or migration coverage', () => {
  assert.deepEqual(
    scopes('pull_request', [['M', 'docs/performance.md'], ['M', 'tests/route_management.rs']]),
    { rust: 'true', migration: 'true', memory: 'false', plugin_installer: 'false' },
  );
});

test('pushes and malformed or empty pull-request diffs fail closed', () => {
  assert.deepEqual(scopes('push', [['M', 'web/src/App.tsx']]), fullPlugin);
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-ci-scope-contract-'));
  try {
    const changed = join(temporary, 'changed.z');
    const output = join(temporary, 'output.txt');
    writeFileSync(output, '');
    writeFileSync(changed, '');
    rejected(process.execPath, ['ops/ci/detect-expensive-ci-scopes.ts', 'pull_request', changed, output]);
    writeFileSync(changed, 'R100\0web/src/old.tsx\0');
    rejected(process.execPath, ['ops/ci/detect-expensive-ci-scopes.ts', 'pull_request', changed, output]);
    writeFileSync(changed, 'X\0web/src/App.tsx\0');
    rejected(process.execPath, ['ops/ci/detect-expensive-ci-scopes.ts', 'pull_request', changed, output]);
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
});
