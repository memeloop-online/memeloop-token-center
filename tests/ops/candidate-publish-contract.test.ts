import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { parse } from 'yaml';
import { REQUIRED_CANDIDATE_JOBS, verifyArtifact, verifyJobs, verifyRun } from '../../scripts/ci/verify-candidate-source.ts';

const revision = 'a'.repeat(40);
const branch = 'candidate/playground-hotfix-20261030';
const digest = `sha256:${'b'.repeat(64)}`;
const run = { repository: { full_name: 'memeloop-online/memeloop-token-center' },
  head_repository: { full_name: 'memeloop-online/memeloop-token-center' }, head_sha: revision, head_branch: branch,
  path: '.github/workflows/ci.yml', event: 'workflow_dispatch', status: 'completed', conclusion: 'success' };
const jobs = REQUIRED_CANDIDATE_JOBS.map((name) => ({ name, status: 'completed', conclusion: 'success' }));
const artifact = { name: `memory-binary-${revision}`, digest, expired: false, id: 123, size_in_bytes: 200,
  workflow_run: { id: 42, head_sha: revision } };

test('candidate source requires exact official full-dispatch identity', () => {
  verifyRun(run, revision, branch);
  for (const change of [{ event: 'pull_request' }, { conclusion: 'failure' }, { status: 'in_progress' },
    { head_sha: 'c'.repeat(40) }, { head_branch: 'master' }, { path: '.github/workflows/manual-release.yml' },
    { head_repository: { full_name: 'attacker/fork' } }, { repository: { full_name: 'attacker/fork' } }]) {
    assert.throws(() => verifyRun({ ...run, ...change }, revision, branch));
  }
});

test('each normal acceptance job must individually succeed without skipped gates', () => {
  verifyJobs(jobs);
  for (let index = 0; index < jobs.length; index++) {
    assert.throws(() => verifyJobs(jobs.filter((_, candidate) => candidate !== index)));
    for (const conclusion of ['failure', 'skipped', 'cancelled', 'neutral']) {
      assert.throws(() => verifyJobs(jobs.map((job, candidate) => candidate === index ? { ...job, conclusion } : job)));
    }
  }
  assert.throws(() => verifyJobs([...jobs, jobs[0]!]));
});

test('artifact identity rejects substitution, expiration and empty input', () => {
  assert.equal(verifyArtifact(artifact, revision, digest, '42'), 123);
  for (const change of [{ expired: true }, { digest: `sha256:${'c'.repeat(64)}` }, { name: 'manual-release-input' },
    { size_in_bytes: 0 }, { id: -1 }, { workflow_run: { id: 43, head_sha: revision } },
    { workflow_run: { id: 42, head_sha: 'c'.repeat(40) } }]) {
    assert.throws(() => verifyArtifact({ ...artifact, ...change }, revision, digest, '42'));
  }
});

type Workflow = { on: Record<string, unknown>; permissions: Record<string, string>; jobs: Record<string, Job> };
type Step = { uses?: string; run?: string; with?: Record<string, unknown> };
type Job = { environment?: string; if?: string; needs?: string[]; permissions?: Record<string, string>; steps: Step[]; strategy?: unknown };
const candidate = parse(readFileSync('.github/workflows/publish-candidate.yml', 'utf8')) as Workflow;
const normal = parse(readFileSync('.github/workflows/ci.yml', 'utf8')) as Workflow;

test('candidate publishing preserves exact existing image scan and verification gates', () => {
  assert.deepEqual(Object.keys(candidate.on), ['workflow_dispatch']);
  assert.deepEqual(candidate.permissions, { contents: 'read' });
  const build = (job: Job) => job.steps.find((step) => step.uses?.startsWith('docker/build-push-action@'))!;
  const scan = (job: Job) => job.steps.find((step) => step.uses?.startsWith('aquasecurity/trivy-action@'))!;
  const publication = candidate.jobs['publish-ghcr']!;
  const existing = normal.jobs['publish-ghcr']!;
  assert.equal(scan(publication).uses, scan(existing).uses);
  const { 'image-ref': _candidateImage, ...candidateScan } = scan(publication).with!;
  const { 'image-ref': _normalImage, ...normalScan } = scan(existing).with!;
  assert.deepEqual(candidateScan, normalScan);
  for (const field of ['outputs', 'platforms', 'provenance', 'sbom']) assert.equal(build(publication).with![field], build(existing).with![field]);
  assert.equal(build(publication).with!.tags, '${{ matrix.image }}:sha-${{ needs.validate-candidate.outputs.revision }}');
  assert.deepEqual(publication.permissions, { contents: 'read', packages: 'write' });
  assert.equal(publication.environment, 'production-candidate');
  for (const job of Object.values(candidate.jobs)) {
    assert.match(job.if!, /github\.ref == 'refs\/heads\/master'/);
    assert.match(job.if!, /github\.ref_protected == true/);
    for (const step of job.steps.filter((step) => step.uses?.startsWith('actions/checkout@'))) {
      assert.equal(step.with!['persist-credentials'], false);
      assert.ok(step.with!.ref === '${{ github.sha }}' || step.with!.ref === '${{ steps.policy.outputs.revision }}');
    }
  }
  const serialized = JSON.stringify(candidate);
  assert.doesNotMatch(serialized, /accept_archive_risk|continue-on-error|workflow_run|pull_request_target/);
  assert.doesNotMatch(serialized, /candidate-source\/(?:scripts|Dockerfile)/);
  assert.match(serialized, /run-candidate-release-tool\.ts verify-published-image/);
  assert.match(serialized, /run-candidate-release-tool\.ts verify-ghcr-release/);
});
