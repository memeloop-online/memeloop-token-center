import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';
import { repository } from './contract-helpers.ts';

const readme = readFileSync(resolve(repository, 'README.md'), 'utf8');

test('README omits deployed addresses and links only to generic project documentation', () => {
  assert.doesNotMatch(readme, /https?:\/\//u);
  assert.doesNotMatch(readme, /token-operator|\.k3s\.|api2-trial|\.svc\.cluster\.local/u);
  assert.match(readme, /\[project overview\]\(docs\/project-overview\.md\)/u);
  assert.match(readme, /approved operational procedures/u);
});

test('README contains no credential-retrieval commands', () => {
  const blocks = [...readme.matchAll(/```([^\n]+)\n([\s\S]*?)\n```/gu)];
  assert.deepEqual(blocks, []);
  assert.doesNotMatch(readme, /kubectl|\bget secret\b|base64|service-token|jsonpath/u);
});
