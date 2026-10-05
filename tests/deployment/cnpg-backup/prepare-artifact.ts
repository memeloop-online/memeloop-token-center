import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { stringify } from 'yaml';
import { boundedJobs, policyName, preparedResources } from './hard-capacity.ts';

export function prepareArtifact(output: string, provenance: { testedCommit: string; sourceHead: string; runId: string }): void {
  assert.match(provenance.testedCommit, /^[a-f0-9]{40}$/);
  assert.match(provenance.sourceHead, /^[a-f0-9]{40}$/);
  assert.match(provenance.runId, /^[0-9]+$/);
  mkdirSync(output, { recursive: false });
  const resources = preparedResources();
  const selected = resources.filter(resource =>
    resource.kind === 'Job' && resource.metadata.name === boundedJobs.stage ||
    resource.kind === 'ConfigMap' && resource.metadata.name === policyName ||
    resource.kind === 'NetworkPolicy' && ['mtc-pg-logical-backup-isolation-20261004', 'mtc-pg-logical-export-egress-20261004'].includes(resource.metadata.name));
  assert.equal(selected.length, 4, 'Export artifact must not allocate storage or include copy/restore Jobs');
  const render = (objects: any[]) => objects.map(resource => stringify(resource)).join('---\n');
  const files: Record<string, string> = {
    'cnpg-hard-capacity-preparation.yaml': render(resources),
    'cnpg-export-preparation.yaml': render(selected),
  };
  for (const name of ['source-space.ts', 'volume-identity.ts']) files[name] = readFileSync(join(dirname(fileURLToPath(import.meta.url)), name), 'utf8');
  const digest = (value: string) => createHash('sha256').update(value).digest('hex');
  files['provenance.json'] = JSON.stringify({
    schema: 1, ...provenance, executionAuthorized: false, exportManifest: 'cnpg-export-preparation.yaml',
    runtime: 'Existing Node 24 and kubectl; no npm install, build, product rollout or cluster credentials in artifact',
    watchdog: 'source-space.ts --watch; separate owner approval required before issuing any export lease',
    sourceGuardAndCsiIdentityRequired: true, copyAndRestoreRootlessIdentityReady: false,
    files: Object.fromEntries(Object.entries(files).map(([name, contents]) => [name, digest(contents)])),
  }, null, 2) + '\n';
  files['SHA256SUMS'] = Object.entries(files).map(([name, contents]) => `${digest(contents)}  ${name}\n`).join('');
  for (const [name, contents] of Object.entries(files)) writeFileSync(join(output, name), contents, { flag: 'wx' });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Generate independently runnable backup artifacts only in GitHub Actions');
  assert.equal(process.argv.length, 3);
  prepareArtifact(process.argv[2]!, {
    testedCommit: execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
    sourceHead: process.env.BACKUP_SOURCE_HEAD ?? '', runId: process.env.GITHUB_RUN_ID ?? '',
  });
}
