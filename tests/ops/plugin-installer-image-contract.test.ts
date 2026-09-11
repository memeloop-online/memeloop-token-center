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

test('plugin installer image is patched, pinned, and non-root', (context) => {
  const dockerfile = 'Dockerfile.plugin-installer';
  for (const needle of [
    'ARG GO_IMAGE=golang:1.26.7-bookworm',
    'https://codeload.github.com/sigstore/cosign/tar.gz/11926fa5bbbbde47e88fc006b625a17769b743b2',
    'sha256:3a718446bac51466efff6853639e1ca108b456ecbf07cd92938f548715d22d6b',
    'COPY packaging/cosign/v3.1.3-security.patch',
    'git apply --unidiff-zero --check /tmp/v3.1.3-security.patch',
    'GOTOOLCHAIN=local go mod verify',
    'ARG RUNTIME_IMAGE=gcr.io/distroless/base-nossl-debian13:nonroot',
    'COPY --from=builder /tmp/libgcc_s.so.1 /usr/local/lib/libgcc_s.so.1',
    'ENV LD_LIBRARY_PATH=/usr/local/lib', 'USER 10001:10001',
    'ENTRYPOINT ["/usr/local/bin/install-plugin-oci"]',
  ]) contains(dockerfile, needle);
  excludes(dockerfile, 'github.com/sigstore/cosign/releases/download/');
  excludes(dockerfile, /^ARG\s+COSIGN_(?:VERSION|SHA|DIGEST|COMMIT)/m);
  excludes('Dockerfile', '/usr/local/bin/cosign');
  excludes('Cargo.toml', /^sigstore\s*=/m);
  contains('packaging/cosign/v3.1.3-security.patch', '+\tgoogle.golang.org/grpc v1.83.2 // indirect');
  contains('packaging/cosign/v3.1.3-security.patch', '+google.golang.org/grpc v1.83.2 h1:EManeRomTObA0BU7I8vXgg/78uE5MJ9M8B39EX2WscU=');
  excludes('packaging/cosign/v3.1.3-security.patch', '+\tgoogle.golang.org/grpc v1.83.1 // indirect');

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
    assert.equal(run('docker', ['image', 'inspect', '--format', '{{index .Config.Labels "io.memeloop.cosign.version"}}', image]).trim(), 'v3.1.3-mtc.3');
    const version = JSON.parse(run('docker', ['run', '--rm', '--entrypoint', '/usr/local/bin/cosign', image, 'version', '--json'])) as { gitVersion?: string; goVersion?: string };
    assert.equal(version.gitVersion, 'v3.1.3-mtc.3');
    assert.equal(version.goVersion, 'go1.26.7');
    run('docker', ['run', '--rm', image, '--help']);
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
