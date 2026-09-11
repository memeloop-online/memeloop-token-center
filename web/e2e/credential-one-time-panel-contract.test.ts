import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const shared = await readFile(new URL('../src/operator/scope/operatorShared.tsx', import.meta.url), 'utf8');

test('one-time credential panel provides copy, local download, failure feedback, and close confirmation', () => {
  assert.match(shared, /navigator\.clipboard\?\.writeText/);
  assert.match(shared, /document\.execCommand\('copy'\)/);
  assert.match(shared, /new Blob\(\[`\$\{value\}\\n`\]/);
  assert.match(shared, /URL\.revokeObjectURL/);
  assert.match(shared, /common\.copySecretFailed/);
  assert.match(shared, /common\.secretSaved/);
  assert.match(shared, /window\.confirm\(t\('common\.confirmDismissSecret'\)\)/);
  assert.match(shared, /onDismiss\(\)/);
  assert.doesNotMatch(shared, /api\(/, 'the panel must never fetch an old secret');
});
