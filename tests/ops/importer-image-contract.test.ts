import assert from 'node:assert/strict';
import { cpSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { occurrences, read, repository, run } from './contract-helpers.ts';
import './test-legacy-policy-import.ts';
import './test-legacy-route-import.ts';

test('importer bundles execute as native ESM without dynamic CommonJS bridges', () => {
  run(process.execPath, ['ops/ci/build-importer-scripts.ts']);
  const upstreamBundle = join(repository, 'dist/operator-scripts/cpa-upstreams/import-cpa-upstreams.mjs');
  assert.doesNotMatch(readFileSync(upstreamBundle, 'utf8'), /Dynamic require of/);
  const localUpstreamHelp = run(process.execPath, [upstreamBundle, '--help']);
  assert.match(localUpstreamHelp, /dry-run by default/);
  assert.match(localUpstreamHelp, /--transport-policy-file/);
  const policyBundle = join(repository, 'dist/operator-scripts/legacy-policy/import-cpa-key-policy.mjs');
  assert.doesNotMatch(readFileSync(policyBundle, 'utf8'), /Dynamic require of/);
  const localPolicyHelp = run(process.execPath, [policyBundle, '--help']);
  assert.match(localPolicyHelp, /dry-run by default/); assert.match(localPolicyHelp, /route-inventory-file FILE --target-api-base-url URL --service-token-file FILE \[--checkpoint-file FILE\]/);
  const routeBundle = join(repository, 'dist/operator-scripts/legacy-routes/import-cpa-model-routes.mjs');
  assert.doesNotMatch(readFileSync(routeBundle, 'utf8'), /Dynamic require of/);
  const localRouteHelp = run(process.execPath, [routeBundle, '--help']);
  assert.match(localRouteHelp, /live dry-run by default/); assert.match(localRouteHelp, /--source-inventory-file FILE --upstream-inventory-file FILE --reviewed-manifest-file FILE/);
  const rollbackBundle = join(repository, 'dist/operator-scripts/api2-target-rollback.mjs');
  assert.doesNotMatch(readFileSync(rollbackBundle, 'utf8'), /Dynamic require of/);
  assert.match(run(process.execPath, [rollbackBundle, '--help']), /Outputs and receipts are never overwritten/);
  const dockerfile = read('Dockerfile.importer');
  assert.match(dockerfile, /apk add --no-cache[^\n]*minio-client/);
  assert.match(dockerfile, /ln -s \/usr\/bin\/mcli \/usr\/local\/bin\/mc/);
  assert.match(dockerfile, /import-cpa-key-policy/);
  assert.match(dockerfile, /import-cpa-model-routes/);
  const policyJob = read('ops/kubernetes/legacy-key-policy-import-job.yaml');
  for (const needle of ['import-mode: dry-run','namespace: REPLACE_TARGET_NAMESPACE','memeloop-token-center-importer@sha256:REPLACE_DIGEST','automountServiceAccountToken: false','readOnlyRootFilesystem: true','runAsUser: 10001','chmod 0600 /runtime/policy.json','REPLACE_SOURCE_POLICY_SHA256','REPLACE_MAPPING_SHA256','REPLACE_ROUTE_INVENTORY_SHA256','REPLACE_POLICY_IMPORT_CHECKPOINT_PVC']) assert.ok(policyJob.includes(needle));
  assert.doesNotMatch(policyJob, /^\s*-\s*--apply\s*$/m);
  const routeJob = read('ops/kubernetes/legacy-route-import-job.yaml');
  for (const needle of ['import-mode: dry-run','namespace: REPLACE_TARGET_NAMESPACE','memeloop-token-center-importer@sha256:REPLACE_DIGEST','automountServiceAccountToken: false','readOnlyRootFilesystem: true','runAsUser: 10001','chmod 0600 /runtime/source-inventory.json','REPLACE_SOURCE_INVENTORY_SHA256','REPLACE_UPSTREAM_INVENTORY_SHA256','REPLACE_REVIEWED_MANIFEST_SHA256','REPLACE_ROUTE_IMPORT_CHECKPOINT_PVC']) assert.ok(routeJob.includes(needle));
  assert.doesNotMatch(routeJob, /^\s*-\s*--apply\s*$/m);
});

test('importer image and migration Jobs are hardened and contain only production assets', (context) => {
  const docker = spawnSync('docker', ['version', '--format', '{{.Client.Version}}'], { encoding: 'utf8', shell: false, stdio: ['ignore', 'pipe', 'pipe'] });
  if (docker.status !== 0) {
    if (process.env.CI === 'true' || process.env.MTC_REQUIRE_DOCKER_CONTRACTS === '1') throw new Error('Docker is required for the importer image contract in CI');
    context.skip('Docker is unavailable; CI requires and executes this image contract');
    return;
  }
  const workspace = mkdtempSync(join(tmpdir(), 'mtc-importer-contract-'));
  const image = process.env.IMPORTER_IMAGE ?? `memeloop-token-center-importer-contract:${process.pid}`;
  const created = !process.env.IMPORTER_IMAGE;
  let container = '';
  const volume = `mtc-cpa-upstream-fixture-${process.pid}`;
  try {
    if (created) {
      const args = ['build', '--file', join(repository, 'Dockerfile.importer'), '--tag', image];
      if (process.env.IMPORTER_RUNTIME_IMAGE) args.push('--build-arg', `RUNTIME_IMAGE=${process.env.IMPORTER_RUNTIME_IMAGE}`);
      run('docker', [...args, repository]);
    }
    assert.equal(run('docker', ['image', 'inspect', image, '--format', '{{.Config.User}}']).trim(), '10001:10001');
    assert.equal(run('docker', ['image', 'inspect', image, '--format', '{{json .Config.Entrypoint}}']).trim(), '["/usr/local/bin/migrate-cpamp"]');
    const runtimeHelper = join(repository, 'tests/ops/helpers/importer-runtime-check.ts');
    run('docker', ['run','--rm','--read-only','--tmpfs','/tmp:rw,noexec,nosuid,size=8m','--security-opt','no-new-privileges','--cap-drop','ALL','--volume',`${runtimeHelper}:/contract/importer-runtime-check.ts:ro`,'--entrypoint','node',image,'/contract/importer-runtime-check.ts']);

    const help = (entrypoint: string): string => run('docker', ['run','--rm','--read-only','--tmpfs','/tmp:rw,noexec,nosuid,size=8m','--security-opt','no-new-privileges','--cap-drop','ALL','--entrypoint',entrypoint,image,'--help']);
    const exporterHelp = help('/usr/local/bin/export-cpa-session-archive-delta');
    assert.match(exporterHelp, /--output/); assert.match(exporterHelp, /--checkpoint/); assert.doesNotMatch(exporterHelp, /(token|credential|ticket).*(argument|value)/i);
    const legacyHelp = help('/usr/local/bin/attach-legacy-cpa-credentials');
    assert.match(legacyHelp, /dry-run by default/); assert.doesNotMatch(legacyHelp, /--credential(?:[ =]|$)/);
    const policyHelp = help('/usr/local/bin/import-cpa-key-policy');
    assert.match(policyHelp, /dry-run by default/); assert.match(policyHelp, /checkpoint/); assert.doesNotMatch(policyHelp, /--(?:key-hash|route-id|service-token)(?:[ =]|$)/);
    const routeHelp = help('/usr/local/bin/import-cpa-model-routes');
    assert.match(routeHelp, /live dry-run by default/); assert.match(routeHelp, /reauthorization is report-only/); assert.doesNotMatch(routeHelp, /--(?:account-id|source-stable-id|service-token)(?:[ =]|$)/);
    const upstreamHelp = help('/usr/local/bin/import-cpa-upstreams');
    assert.match(upstreamHelp, /dry-run by default/); assert.match(upstreamHelp, /--transport-policy-file/); assert.doesNotMatch(upstreamHelp, /--(?:credential|api-key|service-token)(?:[ =]|$)/); assert.doesNotMatch(upstreamHelp, /bridge|subscription-accounts/i);
    const rollbackHelp = help('/usr/local/bin/api2-target-rollback');
    assert.match(rollbackHelp, /restore.*empty new PostgreSQL target/); assert.match(rollbackHelp, /MC_HOST_<alias>/); assert.doesNotMatch(rollbackHelp, /--(?:password|secret|access-key)(?:[ =]|$)/i);
    assert.match(help('/usr/local/bin/mc'), /COMMANDS|USAGE/i);

    const fixture = join(workspace, 'cpa-upstream-source');
    cpSync(join(repository, 'tests/fixtures/cpa-upstreams/supported'), fixture, { recursive: true });
    run('docker', ['volume','create',volume]);
    const fixtureHelper = join(repository, 'tests/ops/helpers/prepare-cpa-upstream-fixture.ts');
    run('docker', ['run','--rm','--user','0:0','--security-opt','no-new-privileges','--cap-drop','ALL','--cap-add','CHOWN','--volume',`${fixture}:/fixture:ro`,'--volume',`${volume}:/source`,'--volume',`${fixtureHelper}:/contract/prepare-cpa-upstream-fixture.ts:ro`,'--entrypoint','node',image,'/contract/prepare-cpa-upstream-fixture.ts']);
    run('docker', ['run','--rm','--user','10001:10001','--read-only','--security-opt','no-new-privileges','--cap-drop','ALL','--volume',`${volume}:/source`,'--entrypoint','/usr/local/bin/generate-source-identity-key',image,'/source/source-identity.key']);
    const dryRun = run('docker', ['run','--rm','--read-only','--tmpfs','/tmp:rw,noexec,nosuid,size=8m','--security-opt','no-new-privileges','--cap-drop','ALL','--volume',`${volume}:/source:ro`,'--entrypoint','/usr/local/bin/import-cpa-upstreams',image,'--config','/source/config.yaml','--auth-dir','/source/auth','--source-identity-key-file','/source/source-identity.key','--transport-policy-file','/source/transport-policy.json']);
    for (const needle of ['"mode":"dry-run"','"api_account_count":6','"private_target_api_account_count":2','"proxied_api_account_count":2','"native_reauthorization_required_count":2']) assert.ok(dryRun.includes(needle));
    assert.doesNotMatch(dryRun, /fixture-only-|Fixture(?:Copilot|Cursor)Handle|example\.test|fixture-proxy\.internal/);

    container = run('docker', ['create','--entrypoint','/bin/true',image]).trim();
    const binaries = ['migrate-cpamp','audit-cpa-migration','import-cpa-session-archive-wrapper','attach-legacy-cpa-credentials','import-cpa-key-policy','import-cpa-model-routes','import-cpa-upstreams','generate-source-identity-key','export-cpa-session-archive-delta','api2-target-rollback'];
    for (const binary of binaries) {
      const destination = join(workspace, binary);
      run('docker', ['cp',`${container}:/usr/local/bin/${binary}`,destination]);
      run(process.execPath, ['--check', destination]);
    }
    for (const sql of ['prepare.sql','reset.sql','apply.sql']) {
      const destination = join(workspace, sql); run('docker', ['cp',`${container}:/usr/local/bin/sql/cpamp/${sql}`,destination]);
      assert.deepEqual(readFileSync(destination), readFileSync(join(repository, 'ops/sql/cpamp', sql)));
    }
    const rootfs = join(workspace, 'rootfs.tar'); run('docker', ['export','--output',rootfs,container]);
    const tar = spawnSync('tar', ['-xOf', rootfs], { cwd: repository, encoding: 'buffer', maxBuffer: 512 * 1024 * 1024, shell: false });
    assert.equal(tar.status, 0, String(tar.stderr));
    assert.doesNotMatch(tar.stdout.toString('latin1'), /fixture-only-cpa-(?:linux-codex|claude-code)-key|fixture-service-token/);

    const stage = read('ops/kubernetes/cpa-upstream-import-dry-run-job.yaml');
    for (const needle of ['automountServiceAccountToken: false','name: REPLACE_IMAGE_PULL_SECRET','name: stage-source-identity-key','import-mode: dry-run','REPLACE_REVIEWED_POLICY_SHA256','REPLACE_APPROVAL_REFERENCE','chmod 0600 /key-runtime/source-identity.key','chown 10001:10001 /key-runtime/source-identity.key','10001:10001:600:1','secretName: REPLACE_TRANSPORT_POLICY_SECRET','medium: Memory','sizeLimit: 1Mi']) assert.ok(stage.includes(needle));
    assert.doesNotMatch(stage, /^\s*-\s*--apply\s*$/m);
    const jobs = ['ops/kubernetes/legacy-credential-import-job.yaml','ops/kubernetes/cpamp-import-job.yaml'];
    for (const path of jobs) {
      const job = read(path);
      for (const needle of ['REPLACE_TENANT_EXTERNAL_ID','memeloop-token-center-importer@sha256:REPLACE_DIGEST','automountServiceAccountToken: false','name: REPLACE_IMAGE_PULL_SECRET','readOnlyRootFilesystem: true','allowPrivilegeEscalation: false','runAsUser: 10001','fsGroup: 10001','name: PGPASSFILE','initContainers:','name: prepare-database-credentials','chmod 0600 /credentials/pgpass','"10001:10001:600"','medium: Memory']) assert.ok(job.includes(needle), `${path} lacks ${needle}`);
      assert.doesNotMatch(job, /^\s*-\s*name:\s*PGPASSWORD\s*$/m); assert.doesNotMatch(job, /memeloop_token_center_dogfood/);
    }
    assert.doesNotMatch(read(jobs[1]!), /image:\s+\S+@sha256:[0-9a-f]{64}/);
  } finally {
    if (container) spawnSync('docker', ['container','rm',container], { stdio: 'ignore', shell: false });
    spawnSync('docker', ['volume','rm',volume], { stdio: 'ignore', shell: false });
    if (created) spawnSync('docker', ['image','rm',image], { stdio: 'ignore', shell: false });
    rmSync(workspace, { recursive: true, force: true });
  }
});
