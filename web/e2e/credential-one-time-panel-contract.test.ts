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
  assert.match(shared, /common\.confirmDismissRecoverableSecret/);
  assert.match(shared, /common\.confirmDismissRecoveredSecret/);
  assert.match(shared, /onDismiss\(\)/);
  assert.match(shared, /finally \{[\s\S]*textarea\.value = ''[\s\S]*textarea\.remove\(\)/, 'the fallback secret node must be cleared even when copying throws');
  assert.match(shared, /objectUrls\.current\.clear\(\)/, 'download URLs must be revoked when the panel unmounts');
  assert.doesNotMatch(shared, /<aside className="one-time" role="status"/, 'a credential must not be announced on mount');
  assert.match(shared, /recovered \? 'common\.recoveredSecretHint' : 'common\.secretShownOnce'/);
  assert.doesNotMatch(shared, /api\(/, 'the panel must never fetch an old secret');
});
