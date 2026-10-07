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
  const expanded = (capacity: 32 | 40 | 56) => render(preparedResources(undefined, capacity).filter(resource =>
    resource.kind === 'Job' && [boundedJobs.stage, boundedJobs.source].includes(resource.metadata.name) ||
    resource.kind === 'ConfigMap' && resource.metadata.name === policyName ||
    resource.kind === 'NetworkPolicy' && ['mtc-pg-logical-backup-isolation-20261004', 'mtc-pg-logical-export-egress-20261004'].includes(resource.metadata.name)));
  const files: Record<string, string> = {
    'cnpg-hard-capacity-preparation.yaml': render(resources),
    'cnpg-export-preparation.yaml': render(selected),
    'cnpg-expanded-stage-preparation.yaml': expanded(32),
    'cnpg-retained-stage-preparation.yaml': expanded(40),
    'cnpg-preserved-partials-stage-preparation.yaml': expanded(56),
  };
  for (const name of ['source-space.ts', 'source-filesystem.ts', 'volume-identity.ts', 'copy.ts', 'copy-guard.ts', 'stage-expansion.ts']) files[name] = readFileSync(join(dirname(fileURLToPath(import.meta.url)), name), 'utf8');
  const digest = (value: string) => createHash('sha256').update(value).digest('hex');
  files['provenance.json'] = JSON.stringify({
    schema: 1, ...provenance, executionAuthorized: false, exportManifest: 'cnpg-export-preparation.yaml',
    expandedStageManifest: 'cnpg-expanded-stage-preparation.yaml', expansionPlanner: 'stage-expansion.ts', expansionAuthorized: false,
    retainedStageManifest: 'cnpg-retained-stage-preparation.yaml', retainedStageCapacityGiB: 40,
    retainedStageBoundary: 'Separate32-to40Gi stage-only expansion review and actual CSI growth required; preserve all failed partials. Source PG,28Gi offhost archive,64Gi scratch,24Gi archive bound,9Gi/8Gi/512Mi source protection,45s lease and default rates unchanged. No allocation or execution authorization.',
    preservedPartialsStageManifest: 'cnpg-preserved-partials-stage-preparation.yaml', preservedPartialsStageCapacityGiB: 56,
    preservedPartialsStageBoundary: 'Separate40-to56Gi stage-only expansion review and actual CSI growth required. Preserve every failed partial on the same filesystem; no source PG resize, partial deletion, new volume, export start or restore authorization. Full24Gi archive cap and start-space reserve, source9Gi/8Gi/512Mi protection and45s leases remain unchanged.',
    expansionBoundary: 'Explicit32Gi stage profile only after separately reviewed physical budget, UID/version-guarded PVC request and actual CSI/filesystem growth. Initial28Gi allocation manifests and old dry-run evidence remain unchanged. No partial deletion, source PG resize, archive cap increase, source budget reset or export approval.',
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
