import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import test from 'node:test';
import { parse } from 'yaml';
import { contains, occurrences, read, repository, run } from './contract-helpers.ts';

type WorkflowStep = { id?: string; if?: string; name?: string; uses?: string; run?: string; with?: Record<string, unknown>; env?: Record<string, unknown> };
type WorkflowJob = { if?: string; needs?: string | string[]; steps?: WorkflowStep[]; uses?: string; with?: Record<string, unknown> };

test('release contains only runtime images and no retired migration delivery surface', () => {
  const dockerfile = read('Dockerfile');
  const compose = read('compose.yaml');
  const minioImage = 'quay.io/minio/minio@sha256:a1ea29fa28355559ef137d71fc570e508a214ec84ff8083e39bc5428980b015e';
  const minioClientImage = 'quay.io/minio/mc@sha256:aead63c77f9db9107f1696fb08ecb0faeda23729cde94b0f663edf4fe09728e3';
  assert.ok(!dockerfile.includes('memeloop-token-center-importer'));
  contains('Dockerfile.plugin-installer', 'FROM ${RUNTIME_IMAGE}');

  const workflow = read('.github/workflows/ci.yml');
  assert.ok(!workflow.includes('Dockerfile.importer'));
  assert.ok(!workflow.includes('memeloop-token-center-importer'));
  assert.ok(!workflow.includes('prepare-cpamp-acceptance-bundle'));
  assert.ok(!workflow.includes('test-cpa-upstream-import'));
  assert.ok(!workflow.includes('legacy_credentials_bulk_postgres'));
  assert.ok(!workflow.includes('test-session-archive-delta-export'));
  assert.equal(occurrences(workflow, minioImage), 1);
  assert.equal(occurrences(compose, minioImage), 1);
  assert.equal(occurrences(workflow, minioClientImage), 1);
  assert.equal(occurrences(compose, minioClientImage), 1);
  assert.ok(!workflow.includes('minio/minio:'));
  assert.ok(!compose.includes('minio/minio:'));
  assert.ok(!workflow.includes('minio/mc:'));
  assert.ok(!compose.includes('minio/mc:'));

  const workflowFiles = readdirSync(new URL('../../.github/workflows/', import.meta.url)).filter((name) => /\.ya?ml$/.test(name));
  const workflows = workflowFiles.map((name) => read(`.github/workflows/${name}`)).join('\n');
  const uses = workflows.split('\n').filter((line) => /^\s*(?:-\s+)?uses:/.test(line));
  assert.ok(uses.length > 0);
  for (const line of uses) assert.match(line, /uses:\s+(?:\.\/\S+|\S+@[0-9a-fA-F]{40}\s+#\s+\S+)/, `unpinned action: ${line}`);
  assert.equal(occurrences(workflows, 'actions/checkout@'), occurrences(workflows, 'persist-credentials: false'));

  const parsed = parse(workflow) as { jobs?: Record<string, WorkflowJob> };
  const memoryBinary = parsed.jobs?.['memory-binary'];
  const memoryAcceptance = parsed.jobs?.['memory-acceptance'];
  const rust = parsed.jobs?.rust;
  const migration = parsed.jobs?.['migration-smoke'];
  assert.equal(memoryBinary?.if, "needs.changes.outputs.memory == 'true'");
  assert.equal(memoryAcceptance?.if, "needs.changes.outputs.memory == 'true'");
  assert.equal(rust?.if, "needs.changes.outputs.rust == 'true'");
  assert.equal(migration?.if, "needs.changes.outputs.migration == 'true'");
  assert.ok(rust?.needs?.includes('changes'));
  assert.ok(migration?.needs?.includes('changes'));
  assert.ok(memoryBinary?.needs?.includes('changes'));
  assert.ok(memoryAcceptance?.needs?.includes('memory-binary'));
  assert.equal(memoryAcceptance?.uses, './.github/workflows/memory-acceptance.yml');
  assert.equal(memoryAcceptance?.with?.binary_artifact, 'memory-binary-${{ github.sha }}');
  for (const jobName of ['repository-security', 'dependency-security', 'web', 'api-contract', 'packaging']) {
    assert.equal(parsed.jobs?.[jobName]?.if, undefined, `${jobName} must remain unconditional`);
  }
  assert.ok(workflow.includes('--find-renames=100%'));

  const publish = parsed.jobs?.['publish-ghcr'];
  assert.ok(publish, 'publish-ghcr job is missing');
  const publishSteps = publish.steps ?? [];
  const buildIndex = publishSteps.findIndex((step) => step.id === 'build');
  assert.ok(buildIndex >= 0, 'publish-ghcr build step is missing');
  const runtimeSmoke = publishSteps[buildIndex + 1];
  assert.equal(runtimeSmoke?.name, 'Start the exact published service image');
  assert.equal(runtimeSmoke?.if, "matrix.cache_scope == 'service'");
  assert.equal(runtimeSmoke?.env?.IMAGE, '${{ matrix.image }}');
  assert.equal(runtimeSmoke?.env?.DIGEST, '${{ steps.build.outputs.digest }}');

  const serializedMatrix = JSON.stringify((parsed.jobs?.['publish-ghcr'] as unknown as { strategy?: unknown })?.strategy);
  assert.match(serializedMatrix, /memeloop-token-center/);
  assert.match(serializedMatrix, /memeloop-token-center-plugin-installer/);
  assert.doesNotMatch(serializedMatrix, /importer/);
  const packaging = parsed.jobs?.packaging;
  const cachedPluginBuild = packaging?.steps?.find((step) => step.name === 'Build the cached hardened plugin installer contract image');
  assert.equal(cachedPluginBuild?.with?.['cache-from'], 'type=gha,scope=plugin-installer');
  assert.equal(cachedPluginBuild?.with?.['cache-to'], 'type=gha,mode=max,scope=plugin-installer');
  assert.equal(cachedPluginBuild?.with?.load, true);
  for (const line of workflow.split('\n').filter((line) => /cargo (?:build|clippy|test|run|tree)(?:\s|$)/.test(line))) assert.ok(line.includes('--locked'), `Cargo command lacks --locked: ${line}`);
  run(process.execPath, ['web/scripts/verify-github-workflow-policy.mjs', '.github/workflows/ci.yml', repository]);
});
