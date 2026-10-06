import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { stringify } from 'yaml';
import { boundedJobs, policyName, preparedResources } from './hard-capacity.ts';
import { copyImage } from './copy-guard.ts';
import { archiveIdentity } from './volume-identity.ts';
import { restoreClaim } from './restore-guard.ts';

export function prepareRestoreArtifact(output: string, provenance: { testedCommit: string; sourceHead: string; runId: string }): void {
  assert.match(provenance.testedCommit, /^[a-f0-9]{40}$/);
  assert.match(provenance.sourceHead, /^[a-f0-9]{40}$/);
  assert.match(provenance.runId, /^[0-9]+$/);
  const resources = preparedResources().filter(resource =>
    resource.kind === 'Job' && resource.metadata.name === boundedJobs.restore ||
    resource.kind === 'ConfigMap' && resource.metadata.name === policyName ||
    resource.kind === 'NetworkPolicy' && resource.metadata.name === 'mtc-pg-restore-isolation-20261004');
  assert.equal(resources.length, 3);
  const command = resources.find(resource => resource.kind === 'Job').spec.template.spec.containers[0].command;
  const digest = (contents: string) => createHash('sha256').update(contents).digest('hex');
  const files: Record<string, string> = {
    'cnpg-restore-preparation.yaml': resources.map(resource => stringify(resource)).join('---\n'),
    'review-required-plan.json': JSON.stringify({
      commandSHA256: digest(JSON.stringify(command)), archiveSHA256: '',
      scratch: { ...archiveIdentity, name: restoreClaim, device: `/dev/longhorn/${restoreClaim}`, capacityGiB: 64,
        claimUID: '', persistentUID: '', longhornUID: '', filesystemUUID: '' },
    }, null, 2) + '\n',
  };
  for (const name of ['restore.ts', 'restore-guard.ts', 'copy-guard.ts', 'volume-identity.ts']) files[name] = readFileSync(join(dirname(fileURLToPath(import.meta.url)), name), 'utf8');
  files['provenance.json'] = JSON.stringify({
    schema: 1, ...provenance, executionAuthorized: false, storageAllocationAuthorized: false,
    image: copyImage, restoreManifest: 'cnpg-restore-preparation.yaml', completeArchiveRequired: true,
    runtime: 'Existing Node 24 and kubectl; independent CSI collectors, owner IPC loss ends renewal; no source database access',
    command: 'PARENT_REVIEW_APPROVED=true node restore.ts EXISTING_RESTORE_POD REVIEWED_PLAN_JSON',
    protection: 'Both volumes require distinct pinned PVC/PV/Longhorn/replica/filesystem identities and independent 45s leases. Template contains no invented scratch UID or archive checksum; incomplete plan is rejected. No allocation, apply, unsuspend, retry, source SQL, ownership/ACL or application acceptance.',
    files: Object.fromEntries(Object.entries(files).map(([name, contents]) => [name, digest(contents)])),
  }, null, 2) + '\n';
  files['SHA256SUMS'] = Object.entries(files).map(([name, contents]) => `${digest(contents)}  ${name}\n`).join('');
  mkdirSync(output, { recursive: false });
  for (const [name, contents] of Object.entries(files)) writeFileSync(join(output, name), contents, { flag: 'wx' });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  assert.equal(process.env.GITHUB_ACTIONS, 'true');
  assert.equal(process.argv.length, 3);
  prepareRestoreArtifact(process.argv[2]!, {
    testedCommit: execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
    sourceHead: process.env.BACKUP_SOURCE_HEAD ?? '', runId: process.env.GITHUB_RUN_ID ?? '',
  });
}
