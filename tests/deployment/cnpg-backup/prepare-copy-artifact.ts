import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { stringify } from 'yaml';
import { boundedJobs, policyName, preparedResources } from './hard-capacity.ts';
import { copyImage } from './copy-guard.ts';

export function prepareCopyArtifact(output: string, provenance: { testedCommit: string; sourceHead: string; runId: string }): void {
  assert.match(provenance.testedCommit, /^[a-f0-9]{40}$/);
  assert.match(provenance.sourceHead, /^[a-f0-9]{40}$/);
  assert.match(provenance.runId, /^[0-9]+$/);
  const resources = preparedResources().filter(resource =>
    resource.kind === 'Job' && [boundedJobs.source, boundedJobs.destination].includes(resource.metadata.name) ||
    resource.kind === 'ConfigMap' && resource.metadata.name === policyName ||
    resource.kind === 'NetworkPolicy' && resource.metadata.name === 'mtc-pg-logical-backup-isolation-20261004');
  assert.equal(resources.length, 4);
  const files: Record<string, string> = {
    'cnpg-copy-preparation.yaml': resources.map(resource => stringify(resource)).join('---\n'),
  };
  for (const name of ['copy.ts', 'copy-guard.ts', 'volume-identity.ts']) files[name] = readFileSync(join(dirname(fileURLToPath(import.meta.url)), name), 'utf8');
  const digest = (contents: string) => createHash('sha256').update(contents).digest('hex');
  files['provenance.json'] = JSON.stringify({
    schema: 1, ...provenance, executionAuthorized: false, copyRootlessIdentityReady: true, restoreRootlessIdentityReady: false,
    image: copyImage, copyManifest: 'cnpg-copy-preparation.yaml', runtime: 'Existing Node 24 and kubectl; no dependencies or product rollout required',
    command: 'PARENT_REVIEW_APPROVED=true BACKUP_ARCHIVE_DIRECTORY=<reviewed unique directory> node copy.ts SOURCE_COPY_POD DESTINATION_COPY_POD SOURCE_SHA256',
    protection: 'Independent CSI watcher per side; elapsed-inclusive renewal cadence and owner IPC disconnect stop renewal. Both copy containers independently exit when the 45s volume lease expires. No source database connection, new storage, or restore.',
    files: Object.fromEntries(Object.entries(files).map(([name, contents]) => [name, digest(contents)])),
  }, null, 2) + '\n';
  files['SHA256SUMS'] = Object.entries(files).map(([name, contents]) => `${digest(contents)}  ${name}\n`).join('');
  mkdirSync(output, { recursive: false });
  for (const [name, contents] of Object.entries(files)) writeFileSync(join(output, name), contents, { flag: 'wx' });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Generate copy artifacts only in GitHub Actions');
  assert.equal(process.argv.length, 3);
  prepareCopyArtifact(process.argv[2]!, {
    testedCommit: execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
    sourceHead: process.env.BACKUP_SOURCE_HEAD ?? '', runId: process.env.GITHUB_RUN_ID ?? '',
  });
}
