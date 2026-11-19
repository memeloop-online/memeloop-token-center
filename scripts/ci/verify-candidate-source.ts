import { createHash } from 'node:crypto';
import { appendFileSync, readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';

type ObjectValue = Record<string, unknown>;
export const REQUIRED_CANDIDATE_JOBS = [
  'changes', 'repository-security', 'dependency-security', 'web', 'rust',
  'rust-suite (suite-1, --partition hash:1/4 -E \'not binary(cucumber)\')',
  'rust-suite (suite-2, --partition hash:2/4 -E \'not binary(cucumber)\')',
  'rust-suite (suite-3, --partition hash:3/4 -E \'not binary(cucumber)\')',
  'rust-suite (suite-4, --partition hash:4/4 -E \'not binary(cucumber)\')',
  'rust-suite (cucumber, -E \'binary(cucumber)\')',
  'migration-smoke', 'api-contract', 'packaging', 'memory-binary',
  'memory-acceptance / optimized-15-minute-harness',
  'release-input-assembly (Dockerfile.release, service)',
  'release-input-assembly (Dockerfile.plugin-installer.release, plugin-installer)',
];
function fail(message: string): never { throw new Error(`candidate source verification: ${message}`); }
function object(value: unknown): ObjectValue {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) fail('expected an object');
  return value as ObjectValue;
}
function array(value: unknown): unknown[] { if (!Array.isArray(value)) fail('expected an array'); return value; }
function same(actual: unknown, expected: unknown, label: string): void { if (actual !== expected) fail(`${label} does not match approved identity`); }
const repository = 'memeloop-online/memeloop-token-center';
export function verifyRun(value: unknown, revision: string, branch: string): void {
  const run = object(value);
  same(object(run.repository).full_name, repository, 'run repository');
  same(object(run.head_repository).full_name, repository, 'head repository');
  same(run.head_sha, revision, 'run revision');
  same(run.head_branch, branch, 'run branch');
  same(run.event, 'workflow_dispatch', 'run event');
  same(run.path, '.github/workflows/ci.yml', 'run workflow');
  same(run.status, 'completed', 'run status');
  same(run.conclusion, 'success', 'run conclusion');
}
export function verifyJobs(values: unknown[]): void {
  for (const name of REQUIRED_CANDIDATE_JOBS) {
    const found = values.map(object).filter((job) => job.name === name);
    if (found.length !== 1) fail(`required job ${name} is missing or duplicated`);
    same(found[0]!.status, 'completed', `${name} status`);
    same(found[0]!.conclusion, 'success', `${name} conclusion`);
  }
}
export function verifyArtifact(value: unknown, revision: string, digest: string, runId: string): number {
  const artifact = object(value);
  same(artifact.name, `memory-binary-${revision}`, 'artifact name');
  same(artifact.digest, digest, 'artifact digest');
  same(artifact.expired, false, 'artifact expiration');
  same(String(object(artifact.workflow_run).id), runId, 'artifact source run');
  same(object(artifact.workflow_run).head_sha, revision, 'artifact source revision');
  if (!Number.isSafeInteger(artifact.id) || Number(artifact.id) <= 0) fail('artifact ID is invalid');
  if (typeof artifact.size_in_bytes !== 'number' || artifact.size_in_bytes <= 0) fail('artifact is empty');
  return Number(artifact.id);
}
async function main(): Promise<void> {
  if (process.env.GITHUB_REPOSITORY !== repository || process.env.GITHUB_REF !== 'refs/heads/master'
      || process.env.GITHUB_EVENT_NAME !== 'workflow_dispatch' || process.env.GITHUB_REF_PROTECTED !== 'true') {
    fail('only a protected official master dispatch is trusted');
  }
  const policy = object(JSON.parse(readFileSync('packaging/candidate-publication.json', 'utf8')));
  const revision = process.env.CANDIDATE_SHA ?? '';
  const runId = process.env.SOURCE_RUN_ID ?? '';
  const digest = process.env.SOURCE_ARTIFACT_DIGEST ?? '';
  if (!/^[0-9a-f]{40}$/.test(revision) || !/^[1-9][0-9]*$/.test(runId) || !/^sha256:[0-9a-f]{64}$/.test(digest)) fail('invalid input');
  same(policy.candidate_sha, revision, 'reviewed candidate revision');
  if (typeof policy.branch !== 'string' || typeof policy.production_base !== 'string'
      || !/^[0-9a-f]{40}$/.test(policy.production_base) || !Array.isArray(policy.allowed_paths)) fail('invalid trusted policy');
  const token = process.env.GH_TOKEN;
  if (!token) fail('GitHub read token is required');
  const api = async (path: string): Promise<unknown> => {
    const response = await fetch(`https://api.github.com/repos/${repository}/${path}`, {
      headers: { Authorization: `Bearer ${token}`, Accept: 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28' },
    });
    if (!response.ok) fail(`GitHub metadata request failed (${response.status})`);
    return response.json();
  };
  const run = await api(`actions/runs/${runId}`);
  verifyRun(run, revision, policy.branch);
  const branch = object(await api(`branches/${encodeURIComponent(policy.branch)}`));
  same(branch.protected, true, 'candidate branch protection');
  same(object(branch.commit).sha, revision, 'current candidate branch');
  const jobs: unknown[] = [];
  for (let page = 1; ; page++) {
    const batch = array(object(await api(`actions/runs/${runId}/jobs?filter=latest&per_page=100&page=${page}`)).jobs);
    jobs.push(...batch);
    if (batch.length < 100) break;
  }
  verifyJobs(jobs);
  const artifacts = array(object(await api(`actions/runs/${runId}/artifacts?per_page=100`)).artifacts).map(object);
  const matches = artifacts.filter((artifact) => artifact.name === `memory-binary-${revision}`);
  if (matches.length !== 1) fail('exact source artifact missing or duplicated');
  const artifactId = verifyArtifact(matches[0], revision, digest, runId);
  const git = (args: string[]): string => {
    const result = spawnSync('git', ['-C', 'candidate-source', ...args], { encoding: 'utf8', shell: false, maxBuffer: 16 * 1024 * 1024 });
    if (result.status !== 0) fail('candidate source identity check failed');
    return result.stdout.trimEnd();
  };
  same(git(['rev-parse', 'HEAD']), revision, 'candidate checkout');
  same(git(['status', '--porcelain=v1', '--untracked-files=all']), '', 'clean candidate checkout');
  git(['merge-base', '--is-ancestor', policy.production_base, revision]);
  same(git(['rev-parse', 'HEAD^{tree}']), policy.candidate_tree, 'reviewed candidate tree');
  const allowedPaths = policy.allowed_paths;
  const paths = git(['diff', '--name-only', policy.production_base, revision]).split('\n');
  if (paths.some((path) => !allowedPaths.includes(path))) fail('candidate changes paths outside the approved delta');
  const delta = git(['diff', '--binary', '--full-index', policy.production_base, revision]);
  const deltaDigest = createHash('sha256').update(`${delta}\n`).digest('hex');
  same(deltaDigest, policy.delta_sha256, 'reviewed complete production delta');
  // Download and hash the immutable ZIP independently: download-artifact only
  // warns on digest mismatch, so its warning cannot be the publication gate.
  const response = await fetch(`https://api.github.com/repos/${repository}/actions/artifacts/${artifactId}/zip`, {
    headers: { Authorization: `Bearer ${token}`, Accept: 'application/vnd.github+json' },
  });
  if (!response.ok || !response.body) fail('source artifact download failed');
  const hash = createHash('sha256');
  let bytes = 0;
  for await (const chunk of response.body) { hash.update(chunk); bytes += chunk.length; }
  same(bytes, matches[0]!.size_in_bytes, 'downloaded artifact size');
  same(`sha256:${hash.digest('hex')}`, digest, 'downloaded artifact digest');
  const evidence = { schema_version: 1, revision, source_run_id: runId, artifact_id: artifactId, artifact_digest: digest,
    production_base: policy.production_base, candidate_tree: policy.candidate_tree, delta_sha256: deltaDigest,
    trusted_workflow_sha: process.env.GITHUB_SHA };
  const { writeFileSync } = await import('node:fs');
  writeFileSync('candidate-source-evidence.json', `${JSON.stringify(evidence, null, 2)}\n`, { flag: 'wx', mode: 0o600 });
  if (!process.env.GITHUB_OUTPUT) fail('GitHub output path is required');
  appendFileSync(process.env.GITHUB_OUTPUT, `revision=${revision}\nartifact_id=${artifactId}\nsource_run_id=${runId}\n`);
  console.log(`Verified approved production candidate ${revision} and exact successful CI artifact`);
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
