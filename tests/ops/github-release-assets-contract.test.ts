import assert from 'node:assert/strict';
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { rejected, repository, run } from './contract-helpers.ts';

const revision = 'a'.repeat(40);
const cargoManifest = readFileSync(join(repository, 'Cargo.toml'), 'utf8');
const releaseVersion = /^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"$/mu.exec(cargoManifest)?.[1];
if (releaseVersion === undefined) throw new Error('Cargo release version is missing');

test('release version stays synchronized across product metadata', () => {
  assert.match(readFileSync(join(repository, 'Cargo.lock'), 'utf8'), new RegExp(`\\[\\[package\\]\\]\\nname = "memeloop-token-center"\\nversion = "${releaseVersion.replaceAll('.', '\\.')}"`));
  for (const file of ['package.json', 'web/package.json', 'web/operator-ui-sdk/package.json']) {
    assert.equal((JSON.parse(readFileSync(join(repository, file), 'utf8')) as { version?: string }).version, releaseVersion, file);
  }
  for (const file of ['package-lock.json', 'web/package-lock.json']) {
    const lock = JSON.parse(readFileSync(join(repository, file), 'utf8')) as { version?: string; packages?: Record<string, { version?: string }> };
    assert.equal(lock.version, releaseVersion, file);
    assert.equal(lock.packages?.['']?.version, releaseVersion, `${file} root package`);
  }
  const chart = readFileSync(join(repository, 'charts/memeloop-token-center/Chart.yaml'), 'utf8');
  assert.ok(chart.includes(`\nversion: ${releaseVersion}\n`));
  assert.ok(chart.includes(`\nappVersion: "${releaseVersion}"\n`));
  assert.ok(readFileSync(join(repository, 'charts/memeloop-token-center/values.yaml'), 'utf8').includes(`\n  tag: "v${releaseVersion}"\n`));
  assert.ok(readFileSync(join(repository, 'openapi/openapi.yaml'), 'utf8').includes(`\n  version: ${releaseVersion}\n`));
});

test('version metadata accepts only the Cargo version tag', () => {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-release-version-'));
  try {
    const output = join(temporary, 'output');
    writeFileSync(output, '');
    run(process.execPath, ['scripts/ci/resolve-release-metadata.ts', `refs/tags/v${releaseVersion}`, output], { cwd: repository });
    const values = Object.fromEntries(readFileSync(output, 'utf8').trim().split('\n').map((line) => line.split('=', 2)));
    assert.deepEqual(values, { version: releaseVersion, version_tag: `v${releaseVersion}`, is_version_tag: 'true' });
    rejected(process.execPath, ['scripts/ci/resolve-release-metadata.ts', `refs/tags/v${releaseVersion}-mismatch`, output], { cwd: repository });
  } finally { rmSync(temporary, { recursive: true, force: true }); }
});

test('GitHub release assets contain raw binaries, runnable archives, checksums, SBOM, and provenance', () => {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-github-release-assets-'));
  try {
    const input = join(temporary, 'input');
    const evidence = join(temporary, 'evidence');
    const output = join(temporary, 'output');
    mkdirSync(input); mkdirSync(evidence);
    for (const name of ['memeloop-token-center', 'install-plugin-oci', 'cosign']) {
      writeFileSync(join(input, name), `#!/bin/sh\necho ${name}\n`); chmodSync(join(input, name), 0o755);
    }
    for (const name of ['libgcc_s.so.1', 'libstdc++.so.6', 'LICENSE', 'THIRD_PARTY_NOTICES.md', 'memory-binary.json']) {
      writeFileSync(join(input, name), `${name}\n`);
    }
    mkdirSync(join(input, 'web')); writeFileSync(join(input, 'web/index.html'), '<!doctype html>\n');
    mkdirSync(join(input, 'third-party-licenses'));
    writeFileSync(join(input, 'third-party-licenses/cosign-LICENSE'), 'Apache-2.0\n');
    writeFileSync(join(input, 'third-party-licenses/rust_decimal-LICENSE'), 'MIT\n');
    writeFileSync(join(input, 'release-service-input.json'), JSON.stringify({ schema_version: 2, revision }));

    for (const scope of ['service', 'plugin-installer']) {
      const directory = join(evidence, `${scope}-attestations`); mkdirSync(directory);
      writeFileSync(join(directory, 'in-toto-spdx.json'), JSON.stringify({ predicateType: 'https://spdx.dev/Document' }));
      writeFileSync(join(directory, 'in-toto-slsa.json'), JSON.stringify({ predicateType: 'https://slsa.dev/provenance/v1' }));
    }
    const manifest = join(temporary, 'release-manifest.json');
    writeFileSync(manifest, JSON.stringify([
      { revision, reference: 'ghcr.io/memeloop-online/memeloop-token-center@sha256:' + '1'.repeat(64) },
      { revision, reference: 'ghcr.io/memeloop-online/memeloop-token-center-plugin-installer@sha256:' + '2'.repeat(64) },
    ]));
    run(process.execPath, ['scripts/ci/create-github-release-assets.ts', input, evidence, manifest, output, revision, releaseVersion], { cwd: repository });
    const names = readdirSync(output).sort();
    for (const required of [
      'SHA256SUMS', 'memeloop-token-center-linux-amd64', 'install-plugin-oci-linux-amd64',
      `memeloop-token-center-${releaseVersion}-linux-amd64.tar.gz`, `install-plugin-oci-${releaseVersion}-linux-amd64.tar.gz`,
      'memeloop-token-center-image-linux-amd64.spdx.json', 'memeloop-token-center-image-linux-amd64.provenance.json',
    ]) assert.ok(names.includes(required), required);
    const serviceListing = run('tar', ['-tzf', join(output, `memeloop-token-center-${releaseVersion}-linux-amd64.tar.gz`)]);
    assert.match(serviceListing, /bin\/memeloop-token-center$/m);
    assert.match(serviceListing, /share\/web\/index\.html$/m);
    assert.match(serviceListing, /third-party-licenses\/cosign-LICENSE$/m);
    const sums = readFileSync(join(output, 'SHA256SUMS'), 'utf8');
    assert.match(sums, /memeloop-token-center-linux-amd64$/m);
    assert.doesNotMatch(sums, /SHA256SUMS|RELEASE\.md/);
  } finally { rmSync(temporary, { recursive: true, force: true }); }
});
