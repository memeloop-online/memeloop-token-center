import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { contains, excludes, repository, run } from './contract-helpers.ts';

function cleanupOwnedImage(
  image: string,
  providedImage: string | undefined,
  removeImage: (name: string) => void = (name) => {
    spawnSync('docker', ['image', 'rm', '--force', name], { cwd: repository, encoding: 'utf8', shell: false });
  },
): void {
  // A supplied image is owned by the caller (the workflow's shared BuildKit
  // result), so this test must not remove it from the local image store.
  if (!providedImage) removeImage(image);
}

test('plugin installer image uses official pinned Cosign and is non-root', (context) => {
  const dockerfile = 'Dockerfile.plugin-installer';
  for (const needle of [
    'ARG GO_IMAGE=golang:1.27.3-bookworm',
    'ARG RUNTIME_IMAGE=gcr.io/distroless/base-nossl-debian13:nonroot',
    'COPY --from=builder /tmp/libgcc_s.so.1 /usr/local/lib/libgcc_s.so.1',
    'ENV LD_LIBRARY_PATH=/usr/local/lib', 'USER 10001:10001',
    'ENTRYPOINT ["/usr/local/bin/install-plugin-oci"]',
  ]) contains(dockerfile, needle);
  excludes(dockerfile, /^ARG\s+COSIGN_(?:VERSION|SHA|DIGEST|COMMIT)/m);
  contains('Dockerfile', 'ARG GO_IMAGE=golang:1.27.3-bookworm');
  contains('Dockerfile', '/usr/local/bin/cosign');
  contains('Dockerfile', '/usr/local/bin/install-plugin-oci');
  contains('src/plugin_runtime_companions.rs', 'COSIGN_VERIFIER_VERSION: &str = "v3.1.4"');
  contains(dockerfile, '--features plugin-distribution,experimental-plugin-revisions');
  excludes('Cargo.toml', /^sigstore\s*=/m);
  for (const builder of ['Dockerfile', dockerfile]) {
    contains(builder, 'https://github.com/sigstore/cosign/releases/download/v3.1.4/cosign-linux-amd64');
    contains(builder, 'sha256:424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa');
    contains(builder, 'https://raw.githubusercontent.com/sigstore/cosign/0e48502cf5ef34e2e60bbd3617aa19945712f705/LICENSE');
    contains(builder, 'sha256:c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4');
    contains(builder, 'go version -m /tmp/cosign');
    contains(builder, 'go1\\.27\\.3$');
    contains(builder, 'github.com/klauspost/compress[[:space:]]+v1\\.20\\.1[[:space:]]');
    excludes(builder, /\bgo\s+build\b/);
    excludes(builder, 'v3.1.3-security.patch');
  }
  contains('.github/workflows/ci.yml', 'docker build --target cosign-builder --tag "$MTC_SIGNED_IMPORT_COSIGN_IMAGE" .');
  contains('src/plugin/application/descriptor_import.rs', 'std::env::var("MTC_SIGNED_IMPORT_COSIGN_IMAGE")');
  contains('src/plugin/application/descriptor_import.rs', '&format!("{container}:/tmp/cosign")');

  if (process.env.MTC_SKIP_DOCKER_BUILD === '1') {
    context.skip('Docker execution is unchanged for this pull request; static image policy was validated');
    return;
  }

  const docker = spawnSync('docker', ['version', '--format', '{{.Client.Version}}'], { encoding: 'utf8', shell: false, stdio: ['ignore', 'pipe', 'pipe'] });
  if (docker.status !== 0) {
    if (process.env.CI === 'true' || process.env.MTC_REQUIRE_DOCKER_CONTRACTS === '1') throw new Error('Docker is required for the plugin installer image contract in CI');
    context.skip('Docker is unavailable; CI requires and executes this image contract');
    return;
  }

  const providedImage = process.env.MTC_PLUGIN_INSTALLER_IMAGE?.trim();
  const image = providedImage || `mtc-plugin-installer-contract:${process.pid}`;
  try {
    if (!providedImage) {
      run('docker', ['build', '--pull', '--file', `${repository}/${dockerfile}`, '--tag', image, repository]);
    }
    assert.equal(run('docker', ['image', 'inspect', '--format', '{{.Config.User}}', image]).trim(), '10001:10001');
    assert.equal(run('docker', ['image', 'inspect', '--format', '{{json .Config.Entrypoint}}', image]).trim(), '["/usr/local/bin/install-plugin-oci"]');
    assert.equal(run('docker', ['image', 'inspect', '--format', '{{index .Config.Labels "io.memeloop.cosign.version"}}', image]).trim(), 'v3.1.4');
    const version = JSON.parse(run('docker', ['run', '--rm', '--entrypoint', '/usr/local/bin/cosign', image, 'version', '--json'])) as { gitVersion?: string; goVersion?: string };
    assert.equal(version.gitVersion, 'v3.1.4');
    assert.equal(version.goVersion, 'go1.27.3');
    const help = run('docker', ['run', '--rm', image, '--help']);
    assert.match(help, /--inventory-file/);
    assert.match(help, /--inventory-entry-file/);
  } finally {
    cleanupOwnedImage(image, providedImage);
  }
});

test('provided plugin installer images remain caller-owned', () => {
  const removed: string[] = [];
  cleanupOwnedImage('caller-provided-image', 'caller-provided-image', (image) => removed.push(image));
  assert.equal(removed.length, 0);

  cleanupOwnedImage('locally-built-image', undefined, (image) => removed.push(image));
  assert.deepEqual(removed, ['locally-built-image']);
});
