import assert from 'node:assert/strict';
import { lstatSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { parse } from 'yaml';
import { contains, occurrences, read, rejected, repository, run } from './contract-helpers.ts';

type WorkflowStep = { name?: unknown; run?: unknown; uses?: unknown; with?: Record<string, unknown> };
type WorkflowJob = { steps?: WorkflowStep[] };

test('release workflow and Docker packaging remain immutable and attested', () => {
  const workflow = read('.github/workflows/ci.yml');
  const memory = read('.github/workflows/memory-acceptance.yml');
  const dockerfile = read('Dockerfile');
  const importer = read('Dockerfile.importer');
  const plugin = read('Dockerfile.plugin-installer');
  assert.ok(!readdirSync(repository).includes('.forgejo'));
  for (const needle of [
    'ARG NODE_IMAGE=node:24.18.0-bookworm-slim', 'ARG RUST_IMAGE=rust:1.95.0-bookworm',
    'ARG RUNTIME_IMAGE=gcr.io/distroless/base-nossl-debian13:nonroot',
    'COPY --from=builder /tmp/libgcc_s.so.1 /usr/local/lib/libgcc_s.so.1',
    'ENV LD_LIBRARY_PATH=/usr/local/lib', 'COPY .cargo/config.toml /build/.cargo/config.toml',
    'COPY vendor ./vendor', 'cargo build --locked --release --bin memeloop-token-center --bin import-cpa-session-archive',
    'cargo clean --release --package memeloop-token-center', 'COPY build.rs ./build.rs',
  ]) assert.ok(dockerfile.includes(needle), `Dockerfile lacks ${needle}`);
  assert.ok(plugin.includes('ARG RUNTIME_IMAGE=gcr.io/distroless/base-nossl-debian13:nonroot'));
  assert.ok(!dockerfile.includes('DEBIAN_MIRROR') && !plugin.includes('DEBIAN_MIRROR'));
  assert.ok(importer.includes('ARG RUNTIME_IMAGE=alpine:3.23.5'));
  for (const needle of [
    'COPY ops/migrate-cpamp.ts ops/audit-cpa-migration.ts ops/import-cpa-session-archive.ts ./ops/',
    'COPY --from=scripts /source/dist/operator-scripts/migrate-cpamp.mjs /usr/local/bin/migrate-cpamp',
    'COPY --from=scripts /source/dist/operator-scripts/audit-cpa-migration.mjs /usr/local/bin/audit-cpa-migration',
    'COPY --from=scripts /source/dist/operator-scripts/sql /usr/local/bin/sql',
    'COPY --from=scripts /source/dist/operator-scripts/export-cpa-session-archive-delta.mjs /usr/local/bin/export-cpa-session-archive-delta',
    'apk upgrade --no-cache libcrypto3 libssl3',
    'apk add --no-cache ca-certificates minio-client nodejs postgresql-client sqlite util-linux',
    'ln -s /usr/bin/mcli /usr/local/bin/mc',
  ]) assert.ok(importer.includes(needle), `Dockerfile.importer lacks ${needle}`);
  assert.ok(!importer.includes('apt-get'));
  const buildLine = dockerfile.indexOf('cargo build --locked');
  assert.ok(dockerfile.indexOf('COPY .cargo/config.toml') < buildLine && dockerfile.indexOf('COPY vendor ./vendor') < buildLine);
  contains('.cargo/config.toml', 'abort_conf:true,background_thread:true,dirty_decay_ms:0,muzzy_decay_ms:0');

  const temporary = mkdtempSync(join(tmpdir(), 'mtc-cpamp-bundle-contract-'));
  try {
    const bundle = join(temporary, 'bundle'); mkdirSync(bundle, { mode: 0o700 });
    run(process.execPath, ['ops/ci/prepare-cpamp-acceptance-bundle.ts', bundle]);
    mkdirSync(join(temporary, 'real-target')); symlinkSync('real-target', join(temporary, 'target-link'));
    rejected(process.execPath, ['ops/ci/prepare-cpamp-acceptance-bundle.ts', join(temporary, 'target-link')]);
    symlinkSync(temporary, join(temporary, 'parent-link'));
    rejected(process.execPath, ['ops/ci/prepare-cpamp-acceptance-bundle.ts', join(temporary, 'parent-link/escape')]);
    assert.deepEqual(readdirSync(bundle).sort(), [
      '0001_initial.sql','0002_query_indexes.sql','0004_request_events.sql','0005_generation_jobs.sql',
      '0018_model_price_tiers.sql','0019_session_archive_import.sql','0021_request_locators.sql',
      '0022_budget_rollups.sql','0023_generation_daily_aggregates.sql','0024_request_stats_rollups.sql',
      '0027_cpamp_source_digests.sql','cpamp-import-postgres-acceptance.test.ts','initial.sql','migrate-cpamp.ts','sql',
    ]);
    assert.equal(lstatSync(bundle).mode & 0o777, 0o555);
    for (const name of readdirSync(bundle).filter((name) => name.endsWith('.sql'))) assert.equal(lstatSync(join(bundle, name)).mode & 0o777, 0o444);
    assert.deepEqual(readFileSync(join(bundle, 'migrate-cpamp.ts')), readFileSync(join(repository, 'ops/migrate-cpamp.ts')));
    for (const name of ['prepare.sql','evaluate.sql','apply.sql','correct-plan.sql','correct.sql','correct-rebuild.sql','reset.sql']) {
      assert.deepEqual(readFileSync(join(bundle, 'sql/cpamp', name)), readFileSync(join(repository, 'ops/sql/cpamp', name)));
    }
  } finally {
    run('chmod', ['-R', 'u+w', temporary]);
    rmSync(temporary, { recursive: true, force: true });
  }

  const workflowFiles = readdirSync(join(repository, '.github/workflows')).filter((name) => /\.ya?ml$/.test(name));
  const workflows = workflowFiles.map((name) => read(`.github/workflows/${name}`)).join('\n');
  const uses = workflows.split('\n').filter((line) => /^\s*(?:-\s+)?uses:/.test(line));
  assert.ok(uses.length > 0);
  for (const line of uses) assert.match(line, /uses:\s+(?:\.\/\S+|\S+@[0-9a-fA-F]{40}\s+#\s+\S+)/, `unpinned action: ${line}`);
  assert.equal(occurrences(workflows, 'actions/checkout@'), occurrences(workflows, 'persist-credentials: false'));
  assert.ok(workflow.includes('node-version: 24.18.0'));
  const parsed = parse(workflow) as { jobs?: Record<string, WorkflowJob> };
  assert.ok(parsed.jobs !== undefined, 'CI workflow jobs are missing');
  for (const [jobName, job] of Object.entries(parsed.jobs)) {
    const steps = job.steps ?? [];
    const typeScriptSteps = steps.flatMap((step, index) => typeof step.run === 'string' && /(?:^|\s)node\s+[^\n]*\.ts(?:\s|$)/mu.test(step.run) ? [index] : []);
    if (typeScriptSteps.length === 0) continue;
    const setupIndex = steps.findIndex((step) => typeof step.uses === 'string' && step.uses.startsWith('actions/setup-node@'));
    assert.ok(setupIndex >= 0 && setupIndex < Math.min(...typeScriptSteps), `${jobName} must set up Node before running TypeScript`);
    assert.equal(String(steps[setupIndex]!.with?.['node-version']), '24.18.0', `${jobName} must pin Node 24.18.0 locally`);
  }
  for (const jobName of ['publish-ghcr', 'verify-ghcr-release']) {
    const releaseJob = parsed.jobs[jobName];
    assert.ok(releaseJob !== undefined, `${jobName} is missing`);
    const runBlocks = (releaseJob.steps ?? []).flatMap((step) => typeof step.run === 'string' ? [step.run] : []);
    for (const runBlock of runBlocks) {
      assert.doesNotMatch(runBlock, /\bcrane\s+(?:digest|manifest|blob)\b|buildx\s+imagetools\s+inspect|\bjq\b|tagged_reference|platform_digest/u,
        `${jobName} must keep GHCR verification logic in tested TypeScript`);
    }
  }
  assert.equal(occurrences(workflow, 'toolchain: 1.95.0'), 4);
  run(process.execPath, ['web/scripts/verify-github-workflow-policy.mjs', '.github/workflows/ci.yml', repository]);
  for (const line of workflow.split('\n').filter((line) => /cargo (?:build|clippy|test|run|tree)(?:\s|$)/.test(line))) assert.ok(line.includes('--locked'), `Cargo command lacks --locked: ${line}`);
  for (const needle of [
    'repository-security:', 'dependency-security:', 'scanners: secret,misconfig',
    'command: check advisories bans licenses sources', 'memory-acceptance:',
    'uses: ./.github/workflows/memory-acceptance.yml',
    'node ops/ci/validate-release-inputs.ts', 'node ops/ci/run-release-source-contracts.ts',
    'ghcr.io/${{ github.repository_owner }}/memeloop-token-center',
    'ghcr.io/${{ github.repository_owner }}/memeloop-token-center-importer',
    'ghcr.io/${{ github.repository_owner }}/memeloop-token-center-plugin-installer',
    "if: github.event_name == 'push' && github.ref == 'refs/heads/master'",
    'MTC_BUILD_GIT_SHA_INPUT=${{ github.sha }}', 'tags: ${{ matrix.image }}:sha-${{ github.sha }}',
    'scanners: vuln', 'image-ref: ${{ matrix.image }}@${{ steps.build.outputs.digest }}',
    'severity: HIGH,CRITICAL', 'provenance: mode=max', 'sbom: true',
    'go-containerregistry/releases/download/v0.21.9/go-containerregistry_Linux_x86_64.tar.gz',
    '5c16d8ddb971cb1d5e6ed8b1e743da8224414eeba2c2762d8f1a61b2f095699e',
    'node ops/ci/verify-published-image.ts "$IMAGE" "$DIGEST" "$CACHE_SCOPE" "$RUNNER_TEMP"',
    'node ops/ci/verify-ghcr-release.ts "$RUNNER_TEMP/release-evidence" "$RUNNER_TEMP/release-manifest.json"',
    'node --test tests/ops/ghcr-release-contract.test.ts',
    'verify-ghcr-release:', 'name: ghcr-release-${{ github.sha }}',
    'DIGEST: ${{ steps.build.outputs.digest }}', 'name: image-digest-${{ matrix.cache_scope }}-${{ github.sha }}',
    'if-no-files-found: error',
    'SQLite migration and replay smoke test', 'PostgreSQL migration and replay smoke test',
    'Exercise CPAMP initial, overlap, incremental, and replay imports',
    'node ops/ci/prepare-cpamp-acceptance-bundle.ts "$acceptance"',
    'chmod -R u+w -- "$acceptance"',
    '--volume "$acceptance:/acceptance:ro"', '/acceptance/cpamp-import-postgres-acceptance.test.ts',
    'cargo fmt --all -- --check', 'cargo clippy --locked --all-targets --all-features -- -D warnings',
    'cargo test --locked --all-targets --all-features', 'npm audit --audit-level=high', 'npm run test:e2e',
    'node --test tests/ops/repository-script-language-contract.test.ts',
  ]) assert.ok(workflow.includes(needle), `CI workflow lacks ${needle}`);
  for (const gate of ['repository-security','dependency-security','web','rust','migration-smoke','api-contract','packaging','memory-acceptance']) assert.ok(workflow.includes(`- ${gate}`));
  assert.ok(memory.includes('workflow_call:') && memory.includes('--profile acceptance') && memory.includes('--gateway-limit-mib 256') && memory.includes('node ops/benchmark-memory.ts'));
  assert.ok(!/tags:.*:(?:master|latest)/.test(workflow));
  assert.ok(!/cpamp-import-postgres-acceptance\.test\.ts:.*acceptance/.test(workflow));
});
