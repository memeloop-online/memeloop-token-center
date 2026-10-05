import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { parseAllDocuments } from 'yaml';
import { prepareArtifact } from './prepare-artifact.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');

test('independent export artifact binds tested revision, includes dependency-free watchdog and never enables execution', context => {
  const parent = mkdtempSync(join(tmpdir(), 'cnpg-artifact-'));
  context.after(() => rmSync(parent, { recursive: true, force: true }));
  const output = join(parent, 'bundle');
  const revision = { testedCommit: '1'.repeat(40), sourceHead: '2'.repeat(40), runId: '37358304060' };
  prepareArtifact(output, revision);
  assert.deepEqual(readdirSync(output).sort(), ['SHA256SUMS', 'cnpg-export-preparation.yaml', 'cnpg-hard-capacity-preparation.yaml', 'provenance.json', 'source-space.ts', 'volume-identity.ts']);
  const manifest = parseAllDocuments(readFileSync(join(output, 'cnpg-export-preparation.yaml'), 'utf8')).map(document => {
    assert.deepEqual(document.errors, []);
    return document.toJS({ maxAliasCount: 0 });
  });
  assert.deepEqual(manifest.map(resource => resource.kind).sort(), ['ConfigMap', 'Job', 'NetworkPolicy', 'NetworkPolicy']);
  const job = manifest.find(resource => resource.kind === 'Job');
  assert.equal(job.metadata.name, 'mtc-pg-bounded-stage-20261005');
  assert.equal(job.spec.suspend, true);
  assert.equal(job.spec.parallelism, 1);
  const environment = job.spec.template.spec.containers[0].env;
  for (const name of ['PARENT_REVIEW_APPROVED', 'SOURCE_IO_REVIEW_APPROVED', 'ROOT_STORAGE_REVIEW_APPROVED', 'HARD_CAPACITY_REVIEW_APPROVED']) {
    assert.equal(environment.find((entry: any) => entry.name === name).value, 'false');
  }
  assert.equal(environment.find((entry: any) => entry.name === 'EXPECTED_BACKUP_FS_UUID').value, '');
  const provenance = JSON.parse(readFileSync(join(output, 'provenance.json'), 'utf8'));
  for (const [name, value] of Object.entries(revision)) assert.equal(provenance[name], value);
  assert.equal(provenance.executionAuthorized, false);
  for (const line of readFileSync(join(output, 'SHA256SUMS'), 'utf8').trim().split('\n')) {
    const [expected, name] = line.split('  ');
    assert.equal(createHash('sha256').update(readFileSync(join(output, name!))).digest('hex'), expected);
  }
  for (const name of ['source-space.ts', 'volume-identity.ts']) {
    const contents = readFileSync(join(output, name), 'utf8');
    for (const imported of contents.matchAll(/from '([^']+)'/g)) assert.ok(imported[1]!.startsWith('node:') || imported[1] === './volume-identity.ts');
  }
  assert.throws(() => prepareArtifact(output, revision), 'Refuse to overwrite a reviewed bundle');
});
