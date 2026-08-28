import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { installExecutableHelper, repository } from './contract-helpers.ts';

const revision = '1'.repeat(40);
const sha = (character: string): string => `sha256:${character.repeat(64)}`;
const names = {
  service: 'memeloop-token-center',
  importer: 'memeloop-token-center-importer',
  'plugin-installer': 'memeloop-token-center-plugin-installer',
} as const;

test('GHCR evidence binds each tag and OCI subject before sealing the three-image release', () => {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-ghcr-release-'));
  try {
    const bin = join(temporary, 'bin'); const fixtures = join(temporary, 'fixtures'); const runner = join(temporary, 'runner');
    mkdirSync(bin); mkdirSync(fixtures); mkdirSync(runner);
    installExecutableHelper('tests/ops/helpers/fake-crane.ts', bin, 'crane');
    installExecutableHelper('tests/ops/helpers/fake-docker.ts', bin, 'docker');
    const mapping: Record<string, string> = {};
    let index = 0;
    for (const [scope, name] of Object.entries(names)) {
      index += 1;
      const image = `ghcr.io/memeloop-online/${name}`;
      const published = sha(String(index)); const subject = sha(String(index + 3));
      const attestation = sha(String(index + 6)); const spdx = sha(String.fromCharCode(96 + index)); const slsa = sha(String.fromCharCode(99 + index));
      const indexFile = `${scope}-index.json`; const attestationFile = `${scope}-attestation.json`;
      const statement = (predicateType: string, predicate: object): object => ({
        _type: 'https://in-toto.io/Statement/v1', predicateType, predicate,
        subject: [{ name: image, digest: { sha256: subject.slice(7) } }],
      });
      writeFileSync(join(fixtures, indexFile), JSON.stringify({
        schemaVersion: 2, mediaType: 'application/vnd.oci.image.index.v1+json', manifests: [
          { mediaType: 'application/vnd.oci.image.manifest.v1+json', digest: subject, platform: { os: 'linux', architecture: 'amd64' }, annotations: {} },
          { mediaType: 'application/vnd.oci.image.manifest.v1+json', digest: attestation, platform: { os: 'unknown', architecture: 'unknown' }, annotations: { 'vnd.docker.reference.type': 'attestation-manifest', 'vnd.docker.reference.digest': subject } },
        ],
      }));
      writeFileSync(join(fixtures, attestationFile), JSON.stringify({
        schemaVersion: 2, mediaType: 'application/vnd.oci.image.manifest.v1+json', artifactType: 'application/vnd.docker.attestation.manifest.v1+json',
        subject: { mediaType: 'application/vnd.oci.image.manifest.v1+json', digest: subject },
        layers: [
          { mediaType: 'application/vnd.in-toto+json', digest: spdx, annotations: { 'in-toto.io/predicate-type': 'https://spdx.dev/Document' } },
          { mediaType: 'application/vnd.in-toto+json', digest: slsa, annotations: { 'in-toto.io/predicate-type': 'https://slsa.dev/provenance/v1' } },
        ],
      }));
      writeFileSync(join(fixtures, `${scope}-spdx.json`), JSON.stringify(statement('https://spdx.dev/Document', { SPDXID: 'SPDXRef-DOCUMENT', spdxVersion: 'SPDX-2.3' })));
      writeFileSync(join(fixtures, `${scope}-slsa.json`), JSON.stringify(statement('https://slsa.dev/provenance/v1', { buildDefinition: {}, runDetails: {} })));
      mapping[`digest|${image}:sha-${revision}`] = published;
      mapping[`manifest|${image}@${published}`] = `file:${indexFile}`;
      mapping[`manifest|${image}@${attestation}`] = `file:${attestationFile}`;
      mapping[`blob|${image}@${spdx}`] = `file:${scope}-spdx.json`;
      mapping[`blob|${image}@${slsa}`] = `file:${scope}-slsa.json`;
    }
    writeFileSync(join(fixtures, 'mapping.json'), JSON.stringify(mapping));
    const dockerLog = join(temporary, 'docker.jsonl');
    const environment = {
      ...process.env,
      PATH: `${bin}:${process.env.PATH ?? ''}`,
      FAKE_CRANE_FIXTURES: fixtures,
      FAKE_DOCKER_LOG: dockerLog,
      FAKE_IMAGE_SOURCE: 'https://github.com/memeloop-online/memeloop-token-center',
      FAKE_IMAGE_REVISION: revision,
      GITHUB_REPOSITORY: 'memeloop-online/memeloop-token-center',
      GITHUB_REPOSITORY_OWNER: 'memeloop-online',
      GITHUB_SERVER_URL: 'https://github.com',
      GITHUB_SHA: revision,
    };
    index = 0;
    for (const [scope, name] of Object.entries(names)) {
      index += 1;
      const result = spawnSync(process.execPath, ['ops/ci/verify-published-image.ts', `ghcr.io/memeloop-online/${name}`, sha(String(index)), scope, runner], {
        cwd: repository, encoding: 'utf8', env: environment, shell: false,
      });
      if (result.status !== 0) {
        const diagnostic = join(temporary, `${scope}-diagnostic`); mkdirSync(diagnostic);
        const direct = spawnSync(process.execPath, [
          'ops/ci/verify-buildkit-attestations.ts', `ghcr.io/memeloop-online/${name}`, sha(String(index)),
          join(temporary, `${scope}-diagnostic-index.json`), diagnostic,
        ], { cwd: repository, encoding: 'utf8', env: environment, shell: false });
        assert.equal(result.status, 0, `${result.stderr}\nDirect attestation diagnostic:\n${direct.stderr}`);
      }
    }
    const release = join(temporary, 'release-manifest.json');
    const complete = spawnSync(process.execPath, ['ops/ci/verify-ghcr-release.ts', runner, release], {
      cwd: repository, encoding: 'utf8', env: environment, shell: false,
    });
    assert.equal(complete.status, 0, complete.stderr);
    const manifest = JSON.parse(readFileSync(release, 'utf8')) as Array<Record<string, unknown>>;
    assert.equal(manifest.length, 3);
    assert.deepEqual(manifest.map((entry) => entry.image), Object.values(names).map((name) => `ghcr.io/memeloop-online/${name}`).sort());
    const dockerCalls = readFileSync(dockerLog, 'utf8').trim().split('\n').map((line) => JSON.parse(line) as string[]);
    assert.equal(dockerCalls.filter((call) => !call.includes('--format')).length, 3, 'every immutable release digest must be inspected remotely');

    const wrongLabelRunner = join(temporary, 'wrong-label-runner'); mkdirSync(wrongLabelRunner);
    const wrongLabel = spawnSync(process.execPath, [
      'ops/ci/verify-published-image.ts', 'ghcr.io/memeloop-online/memeloop-token-center', sha('1'), 'service', wrongLabelRunner,
    ], { cwd: repository, encoding: 'utf8', env: { ...environment, FAKE_IMAGE_REVISION: '2'.repeat(40) }, shell: false });
    assert.notEqual(wrongLabel.status, 0);
    assert.match(wrongLabel.stderr, /source or revision label does not match/);

    const wrongDigestRunner = join(temporary, 'wrong-digest-runner'); mkdirSync(wrongDigestRunner);
    const wrongDigest = spawnSync(process.execPath, [
      'ops/ci/verify-published-image.ts', 'ghcr.io/memeloop-online/memeloop-token-center', sha('f'), 'service', wrongDigestRunner,
    ], { cwd: repository, encoding: 'utf8', env: environment, shell: false });
    assert.notEqual(wrongDigest.status, 0);
    assert.match(wrongDigest.stderr, /tag does not resolve to the build digest/);

    const importerEvidence = join(runner, 'importer-image-digest.json');
    const tampered = JSON.parse(readFileSync(importerEvidence, 'utf8')) as Record<string, unknown>;
    tampered.revision = '2'.repeat(40);
    writeFileSync(importerEvidence, JSON.stringify(tampered), { mode: 0o600 });
    const rejected = spawnSync(process.execPath, ['ops/ci/verify-ghcr-release.ts', runner, join(temporary, 'rejected.json')], {
      cwd: repository, encoding: 'utf8', env: environment, shell: false,
    });
    assert.notEqual(rejected.status, 0);
    assert.match(rejected.stderr, /identity does not match the release/);
  } finally { rmSync(temporary, { recursive: true, force: true }); }
});
