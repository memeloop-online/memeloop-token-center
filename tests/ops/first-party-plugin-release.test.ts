import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, mkdirSync, readdirSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { installerEnvironment } from '../../scripts/ci/resolve-first-party-plugin-installer.ts';
import { checkedCommandOutput } from '../../scripts/ci/first-party-plugin-command-output.ts';
import { firstPartyPackage, packageEnvironment } from '../../scripts/ci/first-party-plugin-package.ts';

const root = new URL('../../', import.meta.url).pathname;
const hash = (bytes: Buffer) => `sha256:${createHash('sha256').update(bytes).digest('hex')}`;

test('typed command-output helper accepts only the reviewed version and an exact digest', () => {
  assert.equal(checkedCommandOutput('cosign-version', { gitVersion: 'v3.1.3-mtc.3' }), '');
  assert.throws(() => checkedCommandOutput('cosign-version', { gitVersion: 'v3.1.3' }));
  const digest = `sha256:${'a'.repeat(64)}`;
  assert.equal(checkedCommandOutput('oras-push', { digest }), digest);
  for (const value of [null, [], {}, { digest: 'latest' }, { digest: `${digest}\nBAD=value` }]) {
    assert.throws(() => checkedCommandOutput('oras-push', value));
  }
  assert.throws(() => checkedCommandOutput('unknown', { digest }));
});

test('installer executable selection is master-reviewed, unavailable by default and not dispatch-controlled', () => {
  const trust = JSON.parse(readFileSync(join(root, '.github/first-party-plugin-installer-trust.json'), 'utf8'));
  assert.throws(() => installerEnvironment({ ...trust, status: 'awaiting-reviewed-installer-release', digest: null, source_revision: null }), /plugin release unavailable/);
  const approved = { ...trust, status: 'ready', digest: `sha256:${'a'.repeat(64)}`, source_revision: 'b'.repeat(40) };
  assert.equal(installerEnvironment(approved), `INSTALLER_SOURCE=${trust.repository}\nINSTALLER_DIGEST=${approved.digest}\nINSTALLER_SOURCE_REVISION=${approved.source_revision}\n`);
  for (const change of [
    { repository: 'ghcr.io/attacker/installer' }, { digest: 'latest' },
    { digest: `${approved.digest}\nGITHUB_TOKEN=attacker` }, { source_revision: null },
    { source_revision: 'master' }, { executable_path: '/tmp/attacker' },
  ]) assert.throws(() => installerEnvironment({ ...approved, ...change }));
  const workflow = readFileSync(join(root, '.github/workflows/publish-first-party-plugin.yml'), 'utf8');
  // Match the existing repository release-packaging contract, including the
  // human-readable annotation beside each immutable action revision.
  for (const line of workflow.split('\n').filter((line) => /^\s*(?:-\s+)?uses:/.test(line))) {
    assert.match(line, /uses:\s+(?:\.\/\S+|\S+@[0-9a-fA-F]{40}\s+#\s+\S+)/);
  }
  assert.doesNotMatch(workflow, /inputs\.(?!package\s*\}\})|installer_digest:/);
  assert(workflow.indexOf('resolve-first-party-plugin-installer.ts') < workflow.indexOf('docker/login-action@'));
  assert(workflow.indexOf('actual_revision=$(docker image inspect') < workflow.indexOf('docker run --rm'));
});

test('dispatch selects only a closed first-party package, never execution-sensitive values', () => {
  for (const name of ['model-guard', 'preferred-account']) {
    const item = firstPartyPackage(name);
    assert.equal(item.id, `mtc-${name}`);
    assert.equal(item.crate, `plugin-sources/${name}`);
    assert.equal(item.source, `ghcr.io/memeloop-online/mtc-${name}`);
    assert.equal(packageEnvironment(name), `PLUGIN_ID=${item.id}\nPLUGIN_CRATE=${item.crate}\nPLUGIN_WASM=${item.wasm}\nPLUGIN_SOURCE=${item.source}\nPLUGIN_HOST_TEST=${item.hostTest}\n`);
  }
  for (const value of [undefined, '', '../preferred-account', '__proto__', 'constructor', 'preferred-account\nPLUGIN_ID=bad', {}, ['model-guard']]) {
    assert.throws(() => firstPartyPackage(value));
  }
  const workflow = readFileSync(join(root, '.github/workflows/publish-first-party-plugin.yml'), 'utf8');
  assert(workflow.indexOf('first-party-plugin-package.ts') < workflow.indexOf('docker/login-action@'));
  assert.match(workflow, /MTC_PREFERRED_ACCOUNT_PACKAGE:/);
  assert.match(workflow, /cargo test --locked --all-features --test "\$PLUGIN_HOST_TEST" -- --ignored/);
  assert.match(workflow, /if \[\[ "\$PLUGIN_ID" == "mtc-preferred-account" \]\]; then\s+cargo test --locked --all-features --lib preferred_account_real_gateway -- --ignored/);
  assert.match(workflow, /- health-intelligence/);
  assert.match(workflow, /PLUGIN_DIGEST: sha256:d9d558a6a6118dadfd2693cfd7f479109dbca53ef8ddae201d8ea70fb93922c3/);
  assert.match(workflow, /UPSTREAM_SIGNING_IDENTITY: https:\/\/github\.com\/memeloop-online\/memeloop-token-center-health-intelligence-plugin\/\.github\/workflows\/publish\.yml@refs\/heads\/master/);
  assert.match(workflow, /inputs\.package == 'health-intelligence'/);
  assert.match(workflow, /cosign verify[\s\S]*"\$UPSTREAM_SIGNING_IDENTITY"[\s\S]*cosign sign --yes "\$PLUGIN_SOURCE@\$PLUGIN_DIGEST"/);
});

test('Model Guard default has no rewrite, provider, or host capability', () => {
  const manifest = JSON.parse(readFileSync(join(root, 'plugin-sources/model-guard/plugin.json'), 'utf8'));
  assert.deepEqual(manifest.capabilities, []);
  assert.deepEqual(manifest.contributions.providers, []);
  assert.equal(manifest.contributions.traffic_policy, true);
  assert.equal(manifest.contributions.request_rewrite, false);
  assert.deepEqual(manifest.contributions.configuration.default, { blocked_models: [] });
  assert.equal(manifest.contributions.configuration.schema.additionalProperties, false);
});

test('Model Guard component exports do not enter the native test cdylib', () => {
  const source = readFileSync(join(root, 'plugin-sources/model-guard/src/lib.rs'), 'utf8');
  assert.match(source, /#\[cfg\(target_arch = "wasm32"\)\]\s*export!\(ModelGuard\);/);
  const workflow = readFileSync(join(root, '.github/workflows/publish-first-party-plugin.yml'), 'utf8');
  assert.match(workflow, /cargo test --locked --manifest-path "\$PLUGIN_CRATE\/Cargo.toml"/);
  assert.match(workflow, /cargo build --locked --release --target wasm32-unknown-unknown --manifest-path "\$PLUGIN_CRATE\/Cargo.toml"/);
});

test('unbuilt plugin sources do not pollute the existing loadable plugin root', () => {
  assert(existsSync(join(root, 'plugin-sources/model-guard/Cargo.toml')));
  for (const entry of readdirSync(join(root, 'plugins'), { withFileTypes: true })) {
    if (!entry.isDirectory() || entry.name.startsWith('.')) continue;
    const packageRoot = join(root, 'plugins', entry.name);
    assert(existsSync(join(packageRoot, 'plugin.json')), `${entry.name} must be an actual package, not a source grouping directory`);
    const manifest = JSON.parse(readFileSync(join(packageRoot, 'plugin.json'), 'utf8'));
    if (manifest.wasm) assert(existsSync(join(packageRoot, manifest.wasm)), `${entry.name} requires its built component`);
  }
});

test('keyless Helm mode omits signing-key Secrets but keeps host-owned policy', () => {
  const flags = ['template', 'plugin-keyless', join(root, 'charts/memeloop-token-center'),
    '--set', 'plugins.runtimeInventory.enabled=true',
    '--set', 'plugins.runtimeInventory.existingClaim=reviewed-rwx',
    '--set', 'plugins.runtimeInventory.installationEnabled=true',
    '--set', 'plugins.runtimeInventory.policyConfigMap=reviewed-policy'];
  const keyless = spawnSync('helm', [...flags, '--set', 'plugins.runtimeInventory.signaturePolicy=cosign-keyless'], { encoding: 'utf8' });
  assert.equal(keyless.status, 0, keyless.stderr);
  assert.match(keyless.stdout, /plugin-runtime-policy/);
  assert.doesNotMatch(keyless.stdout, /plugin-runtime-trust/);
  const legacy = spawnSync('helm', flags, { encoding: 'utf8' });
  assert.notEqual(legacy.status, 0, 'default public-key mode must still require its Secret');
});

for (const name of ['model-guard', 'preferred-account']) test(`${name} release evidence binds manifest and component bytes and rejects tampering`, () => {
  // Synthetic unit-test data only; never published or offered as a release.
  const directory = mkdtempSync(join(tmpdir(), 'mtc-plugin-release-test-'));
  try {
    const selected = firstPartyPackage(name);
    const source = selected.source;
    mkdirSync(join(directory, 'plugin-package'));
    mkdirSync(join(directory, 'plugin-install', selected.id), { recursive: true });
    const files = [
      ['plugin.json', readFileSync(join(root, selected.crate, 'plugin.json'))],
      ['plugin.wasm', Buffer.from('synthetic test bytes')],
    ] as const;
    for (const [name, bytes] of files) writeFileSync(join(directory, 'plugin-package', name), bytes);
    const manifestBytes = Buffer.from(JSON.stringify({
      artifactType: 'application/vnd.memeloop.token-center.plugin.v1',
      config: { mediaType: 'application/vnd.memeloop.token-center.plugin.config.v1+json' },
      layers: files.map(([name, bytes]) => ({ digest: hash(bytes), size: bytes.length, annotations: { 'org.opencontainers.image.title': name } })),
    }));
    const digest = hash(manifestBytes);
    writeFileSync(join(directory, 'plugin-oci-manifest.json'), manifestBytes);
    writeFileSync(join(directory, 'plugin-installation.json'), JSON.stringify({ id: selected.id, version: '1.0.0', digest, source }));
    writeFileSync(join(directory, 'plugin-install', selected.id, '.mtc-oci-install.json'), JSON.stringify({ signature_policy: 'cosign-keyless', digest, source }));
    writeFileSync(join(directory, 'plugin-signature-verification.json'), JSON.stringify([{ critical: { image: { 'docker-manifest-digest': digest } } }]));
    const run = () => spawnSync(process.execPath, [join(root, 'scripts/ci/first-party-plugin-release.ts'), directory], {
      encoding: 'utf8', env: { ...process.env, REQUESTED_PLUGIN_PACKAGE: name, PLUGIN_SOURCE: source, PLUGIN_DIGEST: digest },
    });
    assert.equal(run().status, 0);
    assert.equal(JSON.parse(readFileSync(join(directory, 'plugin-release.json'), 'utf8')).installation_verified, true);
    writeFileSync(join(directory, 'plugin-package/plugin.wasm'), 'changed bytes');
    assert.notEqual(run().status, 0);
    writeFileSync(join(directory, 'plugin-oci-manifest.json'), '{}');
    assert.notEqual(run().status, 0);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
