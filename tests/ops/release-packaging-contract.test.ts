import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import test from 'node:test';
import { parse } from 'yaml';
import { contains, occurrences, read, repository, run } from './contract-helpers.ts';

type WorkflowStep = { id?: string; if?: string; name?: string; uses?: string; run?: string; with?: Record<string, unknown>; env?: Record<string, unknown> };
type WorkflowJob = { steps?: WorkflowStep[] };

test('release contains only runtime images and no retired migration delivery surface', () => {
  const dockerfile = read('Dockerfile');
  assert.ok(!dockerfile.includes('import-cpa-session-archive'));
  assert.ok(!dockerfile.includes('memeloop-token-center-importer'));
  contains('Dockerfile.plugin-installer', 'FROM ${RUNTIME_IMAGE}');

  const workflow = read('.github/workflows/ci.yml');
  assert.ok(!workflow.includes('Dockerfile.importer'));
  assert.ok(!workflow.includes('memeloop-token-center-importer'));
  assert.ok(!workflow.includes('prepare-cpamp-acceptance-bundle'));
  assert.ok(!workflow.includes('test-cpa-upstream-import'));
  assert.ok(!workflow.includes('legacy_credentials_bulk_postgres'));
  assert.ok(!workflow.includes('test-session-archive-delta-export'));

  const workflowFiles = readdirSync(new URL('../../.github/workflows/', import.meta.url)).filter((name) => /\.ya?ml$/.test(name));
  const workflows = workflowFiles.map((name) => read(`.github/workflows/${name}`)).join('\n');
  const uses = workflows.split('\n').filter((line) => /^\s*(?:-\s+)?uses:/.test(line));
  assert.ok(uses.length > 0);
  for (const line of uses) assert.match(line, /uses:\s+(?:\.\/\S+|\S+@[0-9a-fA-F]{40}\s+#\s+\S+)/, `unpinned action: ${line}`);
  assert.equal(occurrences(workflows, 'actions/checkout@'), occurrences(workflows, 'persist-credentials: false'));

  const parsed = parse(workflow) as { jobs?: Record<string, WorkflowJob> };
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
  for (const line of workflow.split('\n').filter((line) => /cargo (?:build|clippy|test|run|tree)(?:\s|$)/.test(line))) assert.ok(line.includes('--locked'), `Cargo command lacks --locked: ${line}`);
  run(process.execPath, ['web/scripts/verify-github-workflow-policy.mjs', '.github/workflows/ci.yml', repository]);
});
