import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { parseAllDocuments } from 'yaml';
import { prepareCopyArtifact } from './prepare-copy-artifact.ts';
import { copyContainerCommand, copyIdentities, copyImage } from './copy-guard.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');

test('copy bundle is standalone, pinned and suspended without database, allocation or restore resources', context => {
  const parent = mkdtempSync(join(tmpdir(), 'cnpg-copy-artifact-'));
  context.after(() => rmSync(parent, { recursive: true, force: true }));
  const output = join(parent, 'bundle');
  const revision = { testedCommit: '1'.repeat(40), sourceHead: '2'.repeat(40), runId: '37481174632' };
  prepareCopyArtifact(output, revision);
  assert.deepEqual(readdirSync(output).sort(), ['SHA256SUMS', 'cnpg-copy-preparation.yaml', 'copy-guard.ts', 'copy.ts', 'provenance.json', 'volume-identity.ts']);
  const manifest = readFileSync(join(output, 'cnpg-copy-preparation.yaml'), 'utf8');
  const objects = parseAllDocuments(manifest).map(document => {
    assert.deepEqual(document.errors, []);
    return document.toJS({ maxAliasCount: 0 });
  });
  assert.deepEqual(objects.map(resource => resource.kind).sort(), ['ConfigMap', 'Job', 'Job', 'NetworkPolicy']);
  for (const side of ['source', 'destination'] as const) {
    const job = objects.find(resource => resource.kind === 'Job' && resource.metadata.name.includes(`copy-${side}`));
    assert.equal(job.spec.suspend, true);
    assert.equal(job.spec.backoffLimit, 0);
    const pod = job.spec.template.spec;
    assert.equal(pod.securityContext.runAsUser, 26);
    assert.equal(pod.automountServiceAccountToken, false);
    const container = pod.containers[0];
    assert.equal(container.image, copyImage);
    assert.equal(container.imagePullPolicy, 'IfNotPresent');
    assert.deepEqual(container.command, ['/bin/sh', '-ec', copyContainerCommand]);
    const env = (name: string) => container.env.find((entry: any) => entry.name === name);
    assert.equal(env('HARD_CAPACITY_REVIEW_APPROVED').value, 'false');
    assert.equal(env('EXPECTED_BACKUP_FS_UUID').value, '');
    assert.equal(env('EXPECTED_BACKUP_DEVICE').value, copyIdentities[side].device);
    assert.equal(env('BACKUP_UUID_ATTESTATION').value, 'external-csi-lease');
    assert.equal(env('POD_UID').valueFrom.fieldRef.fieldPath, 'metadata.uid');
    assert.equal(container.volumeMounts.find((mount: any) => mount.name === 'backup').readOnly, side === 'source');
    assert.equal(pod.volumes.find((volume: any) => volume.name === 'backup').persistentVolumeClaim.readOnly, side === 'source');
    assert.equal(pod.volumes.filter((volume: any) => volume.persistentVolumeClaim).length, 1);
    assert.ok(!container.env.some((entry: any) => entry.valueFrom?.secretKeyRef));
  }
  const network = objects.find(resource => resource.kind === 'NetworkPolicy');
  assert.deepEqual(network.spec.policyTypes, ['Ingress', 'Egress']);
  assert.equal(network.spec.ingress, undefined);
  assert.equal(network.spec.egress, undefined);
  const provenance = JSON.parse(readFileSync(join(output, 'provenance.json'), 'utf8'));
  for (const [name, value] of Object.entries(revision)) assert.equal(provenance[name], value);
  assert.equal(provenance.executionAuthorized, false);
  assert.equal(provenance.restoreRootlessIdentityReady, false);
  for (const line of readFileSync(join(output, 'SHA256SUMS'), 'utf8').trim().split('\n')) {
    const [expected, name] = line.split('  ');
    assert.equal(createHash('sha256').update(readFileSync(join(output, name!))).digest('hex'), expected);
    if (name !== 'provenance.json') assert.equal(provenance.files[name!], expected);
  }
  for (const name of ['copy.ts', 'copy-guard.ts', 'volume-identity.ts']) {
    for (const imported of readFileSync(join(output, name), 'utf8').matchAll(/from '([^']+)'/g)) {
      assert.ok(imported[1]!.startsWith('node:') || ['./copy-guard.ts', './volume-identity.ts'].includes(imported[1]!));
    }
  }
  execFileSync(process.execPath, ['--input-type=module', '-e', "await import('./copy.ts'); await import('./copy-guard.ts');"], { cwd: output, timeout: 10_000 });
  execFileSync('/tmp/kubeconform', ['-strict', '-summary', '-exit-on-error', '-'], { input: manifest, timeout: 90_000 });
  assert.throws(() => prepareCopyArtifact(output, revision));
});
