import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { parseAllDocuments } from 'yaml';

const alertName = 'MTCPostgresPrimaryOrReadWriteEndpointUnavailable';
const sourceCommit = '66efccb5380a772cb89b6999bbe64b7cb6219002';
const sourcePath = 'apps/memeloop-token-center/data/monitoring.yaml';
const fixturePath = new URL('./cnpg-alert.rules.yml', import.meta.url);

function ruleFrom(text: string, source: string): unknown {
  const documents = parseAllDocuments(text);
  for (const document of documents) assert.deepEqual(document.errors, [], `${source} must parse as YAML`);
  const rules = documents.flatMap((document) => {
    const root = document.toJSON() as { spec?: { groups?: Array<{ rules?: unknown[] }> }; groups?: Array<{ rules?: unknown[] }> };
    return (root.spec?.groups ?? root.groups ?? []).flatMap((group) => group.rules ?? []);
  });
  const matches = rules.filter((rule) => (rule as { alert?: string }).alert === alertName);
  assert.equal(matches.length, 1, `${source} must contain exactly one ${alertName} rule`);
  return matches[0];
}

function normalizedRuleBlock(text: string): string {
  const lines = text.replaceAll('\r\n', '\n').trimEnd().split('\n');
  const start = lines.findIndex((line) => line.trimStart() === `- alert: ${alertName}`);
  if (start === -1) throw new Error(`rule block ${alertName} must be present`);
  const firstLine = lines[start];
  if (firstLine === undefined) throw new Error(`rule block ${alertName} must be present`);
  const indentation = firstLine.length - firstLine.trimStart().length;
  const block: string[] = [];
  for (let index = start; index < lines.length; index += 1) {
    const line = lines[index];
    if (line === undefined) throw new Error('rule block line must exist');
    if (index > start && (line === '---' || (line.length - line.trimStart().length === indentation && line.trimStart().startsWith('- ')))) break;
    assert.ok(line.startsWith(' '.repeat(indentation)), 'rule block indentation must remain consistent');
    block.push(line.slice(indentation));
  }
  return `${block.join('\n')}\n`;
}

test('the Prometheus fixture matches the pinned GitOps rule and source hash', () => {
  assert.equal(process.env.GITHUB_ACTIONS, 'true', 'source consistency runs in GitHub Actions');
  assert.ok(process.env.CNPG_GITOPS_SOURCE, 'the workflow must fetch the pinned GitOps source');

  const fixtureText = readFileSync(fixturePath, 'utf8');
  const sourceText = readFileSync(process.env.CNPG_GITOPS_SOURCE, 'utf8');
  assert.match(fixtureText, new RegExp(`^# GitOps source commit: ${sourceCommit}$`, 'm'));
  assert.match(fixtureText, new RegExp(`^# GitOps source path: ${sourcePath.replaceAll('/', '\\/')}$`, 'm'));
  const expectedHash = fixtureText.match(/^# GitOps source rule SHA-256: ([a-f0-9]{64})$/m)?.[1];
  assert.ok(expectedHash, 'fixture must pin the SHA-256 of its source rule block');

  const fixtureRule = ruleFrom(fixtureText, 'fixture');
  const sourceRule = ruleFrom(sourceText, 'GitOps source');
  assert.deepEqual(fixtureRule, sourceRule, 'the fixture must exactly match the pinned GitOps rule');

  for (const [name, text] of [['fixture', fixtureText], ['GitOps source', sourceText]] as const) {
    const actualHash = createHash('sha256').update(normalizedRuleBlock(text)).digest('hex');
    assert.equal(actualHash, expectedHash, `${name} rule block must match the pinned source hash`);
  }
});
