import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const source = await readFile(new URL('../src/operator/UpstreamQuotaReset.tsx', import.meta.url), 'utf8');

test('reset confirmation remains memory-only, scope-bound and non-retrying', () => {
  assert.match(source, /useConfirmDialog\(\[token, tenant, accountId\]\)/);
  assert.match(source, /identity\.current !== scope.*secret\.current = null/);
  assert.match(source, /secret\.current = result\.confirmation_token/);
  assert.match(source, /secret\.current = null/);
  assert.doesNotMatch(source, /localStorage|sessionStorage|console\.|apiRead/);
  assert.match(source, /\['submitted', 'accepted', 'unknown'\]/);
  assert.match(source, /setOperation\(\{ \.\.\.result\.operation, state: 'submitted' \}\)/);
  assert.match(source, /result\.operation\.expires_at <= Date\.now\(\)/);
  assert.match(source, /supplier_defined_codex_rate_limits/);
  assert.match(source, /consumes_credits !== 1/);
});
