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
    'ARG GO_IMAGE=golang:1.26.9-bookworm',
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
  contains('Dockerfile', 'ARG GO_IMAGE=golang:1.26.9-bookworm');
  contains('Dockerfile', '/usr/local/bin/cosign');
  contains('Dockerfile', '/usr/local/bin/install-plugin-oci');
  contains('src/plugin_runtime_companions.rs', 'COSIGN_VERIFIER_VERSION: &str = "v3.1.3-mtc.5"');
  contains(dockerfile, '--features plugin-distribution,experimental-plugin-revisions');
  excludes('Cargo.toml', /^sigstore\s*=/m);
  contains('packaging/cosign/v3.1.3-security.patch', '+\tgoogle.golang.org/grpc v1.83.2 // indirect');
  contains('packaging/cosign/v3.1.3-security.patch', '+google.golang.org/grpc v1.83.2 h1:EManeRomTObA0BU7I8vXgg/78uE5MJ9M8B39EX2WscU=');
  excludes('packaging/cosign/v3.1.3-security.patch', '+\tgoogle.golang.org/grpc v1.83.1 // indirect');
  for (const needle of [
    '+\tgithub.com/klauspost/compress v1.18.7 // indirect',
    '+github.com/klauspost/compress v1.18.7 h1:aUyZsS4kH3QTKurYhAOwAHxllVPnOthb3vPfnF1Ehjw=',
    '+github.com/klauspost/compress v1.18.7/go.mod h1:cwPg85FWrGar70rWktvGQj8/hthj3wpl0PGDogxkrSQ=',
    '+\tgolang.org/x/net v0.60.0 // indirect',
    '+golang.org/x/net v0.60.0 h1:79p50tfZlm0J9YfoDsSi639qSXNGVwEzOPLCxM2FsYU=',
    '+golang.org/x/net v0.60.0/go.mod h1:2DA/G1UfVbCpQPeWTmMPGY7Cs2PkBkwu743bVX5PIVg=',
    '+\tgolang.org/x/crypto v0.57.0',
    '+\tgolang.org/x/sys v0.48.0 // indirect',
    '+\tgolang.org/x/term v0.46.0',
    '+\tgolang.org/x/text v0.42.0 // indirect',
    '+\tgolang.org/x/mod v0.41.0 // indirect',
    '+golang.org/x/mod v0.41.0 h1:qJmnOUb4YB+FsEuM3HcWucdZASCPGhsX6uljO6pog0c=',
    '+golang.org/x/mod v0.41.0/go.mod h1:Ek9pY8RKWXwsWvd3rQiHYtMqkjSUV+s1Rj7j4H5Ur6o=',
    '+\tgolang.org/x/sync v0.23.0',
    '+golang.org/x/sync v0.23.0 h1:KameEIfc1IkluZyXWLn39Wd4tURc6GbCiISGiZm2bQk=',
    '+golang.org/x/sync v0.23.0/go.mod h1:sUUOizhqBxiL6pEWpqNLUiaJn1ShEbZ6BBqskPbjZm0=',
  ]) contains('packaging/cosign/v3.1.3-security.patch', needle);
  excludes('packaging/cosign/v3.1.3-security.patch', /^\+.*github\.com\/klauspost\/compress v1\.18\.6/m);
  excludes('packaging/cosign/v3.1.3-security.patch', /^\+.*golang\.org\/x\/net v0\.58\.0/m);
  for (const builder of ['Dockerfile', dockerfile]) {
    contains(builder, 'go version -m /tmp/cosign');
    contains(builder, 'golang.org/x/net[[:space:]]+v0\\.60\\.0[[:space:]]');
    contains(builder, 'github.com/klauspost/compress[[:space:]]+v1\\.18\\.7[[:space:]]');
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
    assert.equal(run('docker', ['image', 'inspect', '--format', '{{index .Config.Labels "io.memeloop.cosign.version"}}', image]).trim(), 'v3.1.3-mtc.5');
    const version = JSON.parse(run('docker', ['run', '--rm', '--entrypoint', '/usr/local/bin/cosign', image, 'version', '--json'])) as { gitVersion?: string; goVersion?: string };
    assert.equal(version.gitVersion, 'v3.1.3-mtc.5');
    assert.equal(version.goVersion, 'go1.26.9');
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
