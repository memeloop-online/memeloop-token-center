import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { installExecutableHelper, repository } from './contract-helpers.ts';

const image = 'ghcr.io/example/service';
const hash = (character: string): string => `sha256:${character.repeat(64)}`;

test('BuildKit verifier binds OCI index, subject, SPDX, and SLSA evidence', () => {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-attestation-contract-'));
  try {
    const bin = join(temporary, 'bin'); const fixtures = join(temporary, 'fixtures');
    mkdirSync(bin); mkdirSync(fixtures);
    const subject = hash('a'); const attestation = hash('b'); const spdx = hash('c'); const slsa = hash('d');
    const index = {
      schemaVersion: 2, mediaType: 'application/vnd.oci.image.index.v1+json', manifests: [
        { mediaType: 'application/vnd.oci.image.manifest.v1+json', digest: subject, platform: { os: 'linux', architecture: 'amd64' }, annotations: {} },
        { mediaType: 'application/vnd.oci.image.manifest.v1+json', digest: attestation, platform: { os: 'unknown', architecture: 'unknown' }, annotations: { 'vnd.docker.reference.type': 'attestation-manifest', 'vnd.docker.reference.digest': subject } },
      ],
    };
    const manifest = {
      schemaVersion: 2, mediaType: 'application/vnd.oci.image.manifest.v1+json', artifactType: 'application/vnd.docker.attestation.manifest.v1+json',
      subject: { mediaType: 'application/vnd.oci.image.manifest.v1+json', digest: subject },
      layers: [
        { mediaType: 'application/vnd.in-toto+json', digest: spdx, annotations: { 'in-toto.io/predicate-type': 'https://spdx.dev/Document' } },
        { mediaType: 'application/vnd.in-toto+json', digest: slsa, annotations: { 'in-toto.io/predicate-type': 'https://slsa.dev/provenance/v1' } },
      ],
    };
    const statement = (predicateType: string, predicate: object): object => ({
      _type: 'https://in-toto.io/Statement/v1', predicateType, predicate,
      subject: [{ name: image, digest: { sha256: subject.slice(7) } }],
    });
    writeFileSync(join(fixtures, 'index.json'), JSON.stringify(index));
    writeFileSync(join(fixtures, 'attestation.json'), JSON.stringify(manifest));
    writeFileSync(join(fixtures, 'spdx.json'), JSON.stringify(statement('https://spdx.dev/Document', { SPDXID: 'SPDXRef-DOCUMENT', spdxVersion: 'SPDX-2.3' })));
    writeFileSync(join(fixtures, 'slsa.json'), JSON.stringify(statement('https://slsa.dev/provenance/v1', { buildDefinition: {}, runDetails: {} })));
    writeFileSync(join(fixtures, 'mapping.json'), JSON.stringify({
      [`manifest|${image}@${hash('1')}`]: 'file:index.json',
      [`manifest|${image}@${attestation}`]: 'file:attestation.json',
      [`blob|${image}@${spdx}`]: 'file:spdx.json',
      [`blob|${image}@${slsa}`]: 'file:slsa.json',
    }));
    installExecutableHelper('tests/ops/helpers/fake-crane.ts', bin, 'crane');
    const invoke = (evidence: string, indexFile: string) => spawnSync(process.execPath, ['ops/ci/verify-buildkit-attestations.ts', image, hash('1'), indexFile, evidence], {
      cwd: repository, encoding: 'utf8', shell: false, env: { ...process.env, PATH: `${bin}:${process.env.PATH ?? ''}`, FAKE_CRANE_FIXTURES: fixtures },
    });
    const evidence = join(temporary, 'evidence'); mkdirSync(evidence);
    const result = invoke(evidence, join(temporary, 'index-evidence.json'));
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /SBOM and provenance statements verified/);
    assert.ok(readFileSync(join(evidence, `in-toto-${spdx.slice(7)}.json`), 'utf8').includes('SPDXRef-DOCUMENT'));

    (index.manifests[1]!.annotations as Record<string, string>)['vnd.docker.reference.digest'] = hash('e');
    writeFileSync(join(fixtures, 'index.json'), JSON.stringify(index));
    const rejectedEvidence = join(temporary, 'rejected'); mkdirSync(rejectedEvidence);
    const rejected = invoke(rejectedEvidence, join(temporary, 'rejected-index.json'));
    assert.notEqual(rejected.status, 0); assert.match(rejected.stderr, /references a different image manifest/);
  } finally { rmSync(temporary, { recursive: true, force: true }); }
});
