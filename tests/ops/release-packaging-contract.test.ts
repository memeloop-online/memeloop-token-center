import assert from 'node:assert/strict';
import { readdirSync } from 'node:fs';
import test from 'node:test';
import { parse } from 'yaml';
import { contains, occurrences, read, repository, run } from './contract-helpers.ts';

type WorkflowStep = { id?: string; if?: string; name?: string; uses?: string; run?: string; with?: Record<string, unknown>; env?: Record<string, unknown> };
type WorkflowJob = { if?: string; needs?: string | string[]; steps?: WorkflowStep[]; uses?: string; with?: Record<string, unknown> };
type Workflow = {
  concurrency?: { group?: string; 'cancel-in-progress'?: boolean | string };
  jobs?: Record<string, WorkflowJob>;
};

test('release contains only runtime images and no retired migration delivery surface', () => {
  const dockerfile = read('Dockerfile');
  const compose = read('compose.yaml');
  const minioImage = 'quay.io/minio/minio@sha256:a1ea29fa28355559ef137d71fc570e508a214ec84ff8083e39bc5428980b015e';
  const minioClientImage = 'quay.io/minio/mc@sha256:aead63c77f9db9107f1696fb08ecb0faeda23729cde94b0f663edf4fe09728e3';
  assert.ok(!dockerfile.includes('memeloop-token-center-importer'));
  contains('Dockerfile.plugin-installer.release', 'FROM ${RUNTIME_IMAGE}');
  assert.ok(!read('Dockerfile.plugin-installer.release').includes('cargo build'));
  assert.ok(!read('Dockerfile.plugin-installer.release').includes('go build'));

  const workflow = read('.github/workflows/ci.yml');
  const manualRelease = parse(read('.github/workflows/manual-release.yml')) as Workflow;
  const browserSteps = (parse(workflow) as Workflow).jobs?.web?.steps ?? [];
  const cucumber = browserSteps.find((step) => step.run === 'npm run test:e2e');
  assert.equal(cucumber?.env?.MTC_E2E_BINARY, '${{ github.workspace }}/target/debug/memeloop-token-center');
  contains('web/e2e/server.mjs', "'--features', 'experimental-plugin-revisions'");
  assert.ok(!workflow.includes('Dockerfile.importer'));
  assert.ok(!workflow.includes('memeloop-token-center-importer'));
  assert.ok(!workflow.includes('prepare-cpamp-acceptance-bundle'));
  assert.ok(!workflow.includes('test-cpa-upstream-import'));
  assert.ok(!workflow.includes('legacy_credentials_bulk_postgres'));
  assert.ok(!workflow.includes('test-session-archive-delta-export'));
  const manualBuilder = manualRelease.jobs?.['build-release-input'];
  const manualBuild = manualBuilder?.steps?.find((step) => step.uses?.startsWith('docker/build-push-action@'));
  assert.equal(manualBuild?.with?.target, 'release-input-export');
  assert.equal(manualBuild?.with?.outputs, 'type=local,dest=${{ runner.temp }}/release-service-input');
  const manualPublisher = manualRelease.jobs?.['publish-ghcr'];
  assert.deepEqual(manualPublisher?.needs, ['build-release-input']);
  const manualMatrix = JSON.stringify((manualPublisher as unknown as { strategy?: unknown })?.strategy);
  assert.match(manualMatrix, /Dockerfile\.release/);
  assert.match(manualMatrix, /Dockerfile\.plugin-installer\.release/);
  assert.doesNotMatch(manualMatrix, /"Dockerfile"|"Dockerfile\.plugin-installer"/);
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

  const parsed = parse(workflow) as Workflow;
  assert.match(parsed.concurrency?.group ?? '', /^ci-\$\{\{ github\.workflow \}\}-/);
  assert.match(parsed.concurrency?.group ?? '', /github\.event_name == 'pull_request'/);
  assert.match(parsed.concurrency?.group ?? '', /github\.event\.pull_request\.number/);
  assert.match(parsed.concurrency?.group ?? '', /github\.run_id/);
  assert.equal(parsed.concurrency?.['cancel-in-progress'], "${{ github.event_name == 'pull_request' }}");
  const memoryBinary = parsed.jobs?.['memory-binary'];
  const memoryAcceptance = parsed.jobs?.['memory-acceptance'];
  const releaseInputAssembly = parsed.jobs?.['release-input-assembly'];
  const rust = parsed.jobs?.rust;
  const web = parsed.jobs?.web;
  const migration = parsed.jobs?.['migration-smoke'];
  const verifier = parsed.jobs?.['verify-ghcr-release'];
  const hasRuntimeFeature = (command: string): boolean => {
    const args = command.trim().split(/\s+/);
    return args.some((arg, index) => {
      const features = arg === '--features' ? args[index + 1] : arg.startsWith('--features=') ? arg.slice('--features='.length) : undefined;
      return features?.replace(/["']/g, '').split(',').includes('experimental-plugin-revisions') ?? false;
    });
  };
  const imageServiceBuilds = dockerfile.split('\n').filter((line) => /\bcargo\s+build\b/.test(line));
  assert.equal(imageServiceBuilds.length, 2);
  for (const line of imageServiceBuilds) assert.ok(hasRuntimeFeature(line), 'service image must include the installed-revision runtime');
  for (const [name, job] of [['web', web]] as const) {
    const serviceBuilds = job?.steps?.filter((step) => /\bcargo\s+build\b/.test(step.run ?? '')) ?? [];
    assert.equal(serviceBuilds.length, 1, `${name} must build the service binary once`);
    assert.ok(hasRuntimeFeature(serviceBuilds[0]?.run ?? ''), `${name} must test the production service feature set`);
  }
  const memoryDockerBuild = memoryBinary?.steps?.find((step) => step.uses?.startsWith('docker/build-push-action@'));
  assert.equal(memoryDockerBuild?.with?.target, 'release-input-export');
  assert.equal(memoryDockerBuild?.with?.platforms, 'linux/amd64');
  assert.equal(memoryDockerBuild?.with?.outputs, 'type=local,dest=${{ runner.temp }}/release-service-input');
  assert.equal(memoryDockerBuild?.with?.['cache-from'], 'type=gha,scope=service-release-input');
  assert.equal(memoryDockerBuild?.with?.['cache-to'], 'type=gha,mode=max,scope=service-release-input');
  assert.match(String(memoryDockerBuild?.with?.['build-args']), /MTC_BUILD_GIT_SHA_INPUT=\$\{\{ github\.sha \}\}/);
  assert.match(String(memoryDockerBuild?.with?.['build-args']), /MTC_BUILD_TARGET_INPUT=\$\{\{ steps\.release-input\.outputs\.target \}\}/);
  contains('Dockerfile', 'FROM scratch AS release-input-export');
  contains('Dockerfile', 'FROM ${RUNTIME_IMAGE} AS release-input-smoke');
  contains('Dockerfile.release', 'COPY --chmod=0555 --from=release-input /memeloop-token-center /usr/local/bin/memeloop-token-center');
  contains('Dockerfile', 'COPY --from=web-builder /build/web/dist /release-input/web');
  contains('Dockerfile.release', 'COPY --from=release-input /web /usr/share/memeloop-token-center/web');
  assert.ok(!read('Dockerfile.release').includes('npm run build'));
  assert.equal(memoryBinary?.if, "needs.changes.outputs.memory == 'true'");
  assert.equal(memoryAcceptance?.if, "needs.changes.outputs.memory_acceptance == 'true'");
  assert.equal(rust?.if, "needs.changes.outputs.rust == 'true'");
  assert.equal(web?.if, "needs.changes.outputs.web == 'true'");
  assert.equal(migration?.if, "needs.changes.outputs.migration == 'true'");
  assert.ok(web?.needs?.includes('changes'));
  assert.ok(rust?.needs?.includes('changes'));
  assert.ok(migration?.needs?.includes('changes'));
  assert.ok(memoryBinary?.needs?.includes('changes'));
  assert.ok(memoryAcceptance?.needs?.includes('memory-binary'));
  const memoryAcceptanceNeeds = Array.isArray(memoryAcceptance?.needs)
    ? memoryAcceptance.needs
    : memoryAcceptance?.needs === undefined ? [] : [memoryAcceptance.needs];
  assert.deepEqual(new Set(memoryAcceptanceNeeds), new Set(['changes', 'memory-binary']));
  assert.equal(memoryAcceptance?.uses, './.github/workflows/memory-acceptance.yml');
  assert.equal(memoryAcceptance?.with?.binary_artifact, 'memory-binary-${{ github.sha }}');
  assert.equal(releaseInputAssembly?.if, "github.event_name == 'pull_request' && needs.changes.outputs.memory == 'true'");
  const releaseInputAssemblyNeeds = Array.isArray(releaseInputAssembly?.needs)
    ? releaseInputAssembly.needs
    : releaseInputAssembly?.needs === undefined ? [] : [releaseInputAssembly.needs];
  assert.deepEqual(new Set(releaseInputAssemblyNeeds), new Set(['changes', 'memory-binary']));
  const assemblyDownload = releaseInputAssembly?.steps?.find((step) => step.uses?.startsWith('actions/download-artifact@'));
  assert.equal(assemblyDownload?.with?.name, 'memory-binary-${{ github.sha }}');
  assert.equal(assemblyDownload?.with?.path, '${{ runner.temp }}/release-service-input');
  const assemblyBuild = releaseInputAssembly?.steps?.find((step) => step.uses?.startsWith('docker/build-push-action@'));
  assert.equal(assemblyBuild?.with?.file, '${{ matrix.dockerfile }}');
  assert.equal(assemblyBuild?.with?.outputs, 'type=cacheonly');
  assert.match(String(assemblyBuild?.with?.['build-contexts']), /^release-input=\$\{\{ runner\.temp \}\}\/release-service-input\s*$/);
  assert.match(JSON.stringify((releaseInputAssembly as unknown as { strategy?: unknown })?.strategy), /Dockerfile\.plugin-installer\.release/);
  for (const jobName of ['repository-security', 'dependency-security', 'api-contract', 'packaging']) {
    assert.equal(parsed.jobs?.[jobName]?.if, undefined, `${jobName} must remain unconditional`);
  }
  const scope = parsed.jobs?.changes?.steps?.find((step) => step.id === 'scope');
  assert.equal(scope?.env?.PR_HEAD_SHA, '${{ github.event.pull_request.head.sha }}');
  assert.ok(scope?.run?.includes('detect-expensive-ci-scopes.ts "$EVENT_NAME" --verified-merge "$GITHUB_OUTPUT"'));
  assert.ok(read('scripts/ci/detect-expensive-ci-scopes.ts').includes('--find-renames=100%'));

  const publish = parsed.jobs?.['publish-ghcr'];
  assert.ok(publish, 'publish-ghcr job is missing');
  assert.deepEqual(verifier?.needs, ['publish-ghcr']);
  assert.match(
    String(verifier?.if).replace(/\s+/g, ' ').trim(),
    /^always\(\) && github\.event_name == 'push' && \(github\.ref == 'refs\/heads\/master' \|\| startsWith\(github\.ref, 'refs\/tags\/v'\)\) && needs\.publish-ghcr\.result == 'success'$/,
    'verify-ghcr-release must not inherit skipped-ancestor propagation from publish prerequisites',
  );
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
  const servicePublisher = publishSteps.find((step) => step.id === 'build');
  assert.match(String(servicePublisher?.with?.['build-contexts']), /^release-input=\$\{\{ runner\.temp \}\}\/release-service-input\s*$/);
  assert.match(serializedMatrix, /Dockerfile\.release/);
  assert.match(serializedMatrix, /Dockerfile\.plugin-installer\.release/);
  const githubRelease = parsed.jobs?.['publish-github-release'];
  assert.deepEqual(new Set(Array.isArray(githubRelease?.needs) ? githubRelease.needs : []), new Set(['changes', 'memory-binary', 'verify-ghcr-release']));
  assert.match(String(githubRelease?.if), /is_version_tag == 'true'/);
  assert.ok(githubRelease?.steps?.some((step) => step.run?.includes('create-github-release-assets.ts')));
  assert.ok(githubRelease?.steps?.some((step) => step.run?.includes('gh release create')));
  const packaging = parsed.jobs?.packaging;
  const cachedPluginBuild = packaging?.steps?.find((step) => step.name === 'Build the cached hardened plugin installer contract image');
  assert.equal(cachedPluginBuild?.with?.['cache-from'], 'type=gha,scope=plugin-installer');
  assert.equal(cachedPluginBuild?.with?.['cache-to'], 'type=gha,mode=max,scope=plugin-installer');
  assert.equal(cachedPluginBuild?.with?.load, true);
  for (const line of workflow.split('\n').filter((line) => /cargo (?:build|clippy|test|run|tree)(?:\s|$)/.test(line))) assert.ok(line.includes('--locked'), `Cargo command lacks --locked: ${line}`);
  run(process.execPath, ['web/scripts/verify-github-workflow-policy.mjs', '.github/workflows/ci.yml', repository]);
});
