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
  assert.deepEqual(readdirSync(output).sort(), ['SHA256SUMS', 'cnpg-expanded-stage-preparation.yaml', 'cnpg-export-preparation.yaml', 'cnpg-hard-capacity-preparation.yaml', 'cnpg-preserved-partials-stage-preparation.yaml', 'cnpg-retained-stage-preparation.yaml', 'copy-guard.ts', 'copy.ts', 'provenance.json', 'source-filesystem.ts', 'source-space.ts', 'stage-expansion.ts', 'volume-identity.ts']);
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
  assert.equal(provenance.expansionAuthorized, false);
  const expanded = parseAllDocuments(readFileSync(join(output, provenance.expandedStageManifest), 'utf8')).map(document => {
    assert.deepEqual(document.errors, []);
    return document.toJS({ maxAliasCount: 0 });
  });
  assert.deepEqual(expanded.map(resource => resource.kind).sort(), ['ConfigMap', 'Job', 'Job', 'NetworkPolicy', 'NetworkPolicy']);
  for (const resource of expanded.filter(entry => entry.kind === 'Job')) {
    assert.equal(resource.spec.suspend, true);
    const environment = resource.spec.template.spec.containers[0].env;
    const value = (name: string) => environment.find((entry: any) => entry.name === name)?.value;
    assert.equal(value('REVIEWED_STAGE_CAPACITY_GIB'), '32');
    assert.equal(value('BACKUP_MAX_BYTES'), String(32 * 1024 ** 3));
    assert.equal(value('BACKUP_MIN_BYTES'), String(30 * 1024 ** 3));
    assert.equal(value('HARD_CAPACITY_REVIEW_APPROVED'), 'false');
    if (resource.metadata.name.includes('bounded-stage')) assert.equal(value('PARENT_REVIEW_APPROVED'), 'false');
    else {
      assert.equal(value('PARENT_REVIEW_APPROVED'), undefined);
      assert.equal(resource.spec.template.spec.volumes.find((entry: any) => entry.name === 'backup').persistentVolumeClaim.readOnly, true);
      assert.ok(resource.spec.template.spec.containers[0].command[2].includes('capacity_backup || exit 1'));
    }
  }
  const expandedExport = expanded.find(resource => resource.kind === 'Job' && resource.metadata.name.includes('bounded-stage'));
  assert.equal(expandedExport.spec.template.spec.containers[0].env.find((entry: any) => entry.name === 'BACKUP_RATE_MIB_PER_SECOND').value, '1');
  assert.ok(expandedExport.spec.template.spec.containers[0].command[6].includes('--fsize=25769803776:25769803776'));
  assert.ok(expandedExport.spec.template.spec.containers[0].command[6].includes('26071793664'));
  assert.equal(provenance.retainedStageCapacityGiB, 40);
  const retained = parseAllDocuments(readFileSync(join(output, provenance.retainedStageManifest), 'utf8')).map(document => {
    assert.deepEqual(document.errors, []);
    return document.toJS({ maxAliasCount: 0 });
  });
  const expectedRetained = structuredClone(expanded);
  for (const resource of expectedRetained.filter(entry => entry.kind === 'Job')) {
    resource.metadata.annotations['recovery.mtc/stage-capacity-profile'] = 'stage40-after-separate-reviewed-expansion-no-source-pg-resize-or-partial-delete';
    const environment = resource.spec.template.spec.containers[0].env;
    for (const [name, value] of [['REVIEWED_STAGE_CAPACITY_GIB', '40'], ['BACKUP_MAX_BYTES', String(40 * 1024 ** 3)], ['BACKUP_MIN_BYTES', String(38 * 1024 ** 3)]]) environment.find((entry: any) => entry.name === name).value = value;
  }
  assert.deepEqual(retained, expectedRetained, 'Only the reviewed stage capacity profile may differ; commands, caps, rates and approvals stay unchanged');
  assert.equal(provenance.preservedPartialsStageCapacityGiB, 56);
  const preservedPartials = parseAllDocuments(readFileSync(join(output, provenance.preservedPartialsStageManifest), 'utf8')).map(document => {
    assert.deepEqual(document.errors, []);
    return document.toJS({ maxAliasCount: 0 });
  });
  const expectedPreserved = structuredClone(retained);
  for (const resource of expectedPreserved.filter(entry => entry.kind === 'Job')) {
    resource.metadata.annotations['recovery.mtc/stage-capacity-profile'] = 'stage56-after-separate-reviewed-expansion-no-source-pg-resize-or-partial-delete';
    const environment = resource.spec.template.spec.containers[0].env;
    for (const [name, value] of [['REVIEWED_STAGE_CAPACITY_GIB', '56'], ['BACKUP_MAX_BYTES', String(56 * 1024 ** 3)], ['BACKUP_MIN_BYTES', String(54 * 1024 ** 3)]]) environment.find((entry: any) => entry.name === name).value = value;
  }
  assert.deepEqual(preservedPartials, expectedPreserved, 'Retaining partials changes only capacity assertions, never scripts, source protection or execution approval');
  for (const line of readFileSync(join(output, 'SHA256SUMS'), 'utf8').trim().split('\n')) {
    const [expected, name] = line.split('  ');
    assert.equal(createHash('sha256').update(readFileSync(join(output, name!))).digest('hex'), expected);
  }
  for (const name of ['source-space.ts', 'source-filesystem.ts', 'volume-identity.ts', 'copy.ts', 'copy-guard.ts', 'stage-expansion.ts']) {
    const contents = readFileSync(join(output, name), 'utf8');
    for (const imported of contents.matchAll(/from '([^']+)'/g)) assert.ok(imported[1]!.startsWith('node:') || ['./volume-identity.ts', './source-filesystem.ts', './copy-guard.ts'].includes(imported[1]!));
  }
  assert.throws(() => prepareArtifact(output, revision), 'Refuse to overwrite a reviewed bundle');
});
