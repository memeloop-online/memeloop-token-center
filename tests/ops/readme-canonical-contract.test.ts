import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';
import { repository } from './contract-helpers.ts';

const readme = readFileSync(resolve(repository, 'README.md'), 'utf8');

test('README exposes only canonical API, Portal and private Operator addresses', () => {
  const urls = [...readme.matchAll(/https?:\/\/[^\s`)]+/gu)].map(([url]) => url);
  assert.deepEqual(urls.sort(), [
    'https://token.k3s.onetwo.website/v1',
    'https://token.api.onetwo.website/v1',
    'https://token.k3s.onetwo.website/portal',
    'https://token.api.onetwo.website/portal',
    'https://token-operator.k3s.onetwo.website/operator',
    'https://token-operator.k3s.onetwo.website/internal/v1',
  ].sort());
  assert.doesNotMatch(readme, /token\.api[23]\.|token-center\.(?:api|k3s)\.|localhost|127\.0\.0\.1|\.svc\.cluster\.local/u);
  assert.match(readme, /Client credentials cannot access the management API/u);
  assert.match(readme, /\[project overview\]\(docs\/project-overview\.md\)/u);
});

test('README management commands are single-line read-only lookups with failure guards', () => {
  const blocks = [...readme.matchAll(/```([^\n]+)\n([\s\S]*?)\n```/gu)];
  assert.deepEqual(blocks.map((block) => block[1]), ['bash', 'powershell']);
  for (const [, , command] of blocks) {
    assert.equal(typeof command, 'string');
    assert.ok(command);
    assert.equal(command.split('\n').length, 1);
    assert.match(command, /kubectl -n memeloop-token-center-api2-trial get secret memeloop-token-center-secrets/u);
    assert.match(command, /jsonpath=\{\.data\.service-token\}/u);
    assert.doesNotMatch(command, /\b(?:create|apply|patch|delete|replace|rotate|openssl|curl)\b/u);
  }
  const bash = blocks[0]?.[2];
  const powershell = blocks[1]?.[2];
  assert.ok(bash);
  assert.ok(powershell);
  assert.match(bash, /set -euo pipefail/u);
  assert.match(bash, /test -n "\$mtc_service_token_b64"/u);
  assert.match(bash, /base64 --decode/u);
  assert.match(powershell, /\$LASTEXITCODE -ne 0/u);
  assert.match(powershell, /IsNullOrWhiteSpace/u);
  assert.match(powershell, /FromBase64String/u);
  assert.match(readme, /do not run them in CI/u);
});
