import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { lstatSync, mkdirSync, openSync, realpathSync, writeFileSync, closeSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type ObjectJson = { [key: string]: Json };

function fail(message: string): never { throw new Error(`BuildKit attestation verification: ${message}`); }
function object(value: Json | undefined, label: string): ObjectJson {
  if (value === null || Array.isArray(value) || typeof value !== 'object') fail(`${label} is not an object`);
  return value as ObjectJson;
}
function array(value: Json | undefined, label: string): Json[] {
  if (!Array.isArray(value)) fail(`${label} is not an array`);
  return value;
}
function digest(value: Json | undefined, label: string): string {
  if (typeof value !== 'string' || !/^sha256:[0-9a-f]{64}$/.test(value)) fail(`${label} is not a lowercase sha256 digest`);
  return value;
}
function text(value: Json | undefined, label: string): string {
  if (typeof value !== 'string') fail(`${label} is not a string`);
  return value;
}
function crane(args: string[]): string {
  const result = spawnSync('crane', args, { encoding: 'utf8', shell: false, maxBuffer: 128 * 1024 * 1024 });
  if (result.status !== 0) fail(`crane ${args[0]} failed: ${result.stderr.trim()}`);
  return result.stdout;
}
function parse(payload: string, label: string): ObjectJson {
  try { return object(JSON.parse(payload) as Json, label); } catch (error) { fail(`${label} is invalid JSON: ${String(error)}`); }
}
function writeExclusive(path: string, payload: string): void {
  const descriptor = openSync(path, 'wx', 0o600);
  try { writeFileSync(descriptor, payload, 'utf8'); } finally { closeSync(descriptor); }
}

const [image = '', indexDigest = '', indexPath = '', evidenceDirectory = ''] = process.argv.slice(2);
if (!/^[a-z0-9][a-z0-9./:_-]*$/.test(image) || image.includes('@')) fail('image repository is invalid');
digest(indexDigest, 'published index digest');
if (indexPath === '') fail('index evidence path is required');
const evidenceStat = lstatSync(evidenceDirectory);
if (!evidenceStat.isDirectory() || evidenceStat.isSymbolicLink() || realpathSync(evidenceDirectory) !== resolve(evidenceDirectory)) {
  fail('evidence directory must be an existing canonical non-symlink directory');
}
if (realpathSync(dirname(indexPath)) !== resolve(dirname(indexPath))) fail('index evidence parent must be canonical');

const indexPayload = crane(['manifest', `${image}@${indexDigest}`]);
const index = parse(indexPayload, 'published OCI index');
assert.equal(index.schemaVersion, 2, 'OCI index schemaVersion');
if (index.mediaType !== 'application/vnd.oci.image.index.v1+json') fail('published digest is not an OCI image index');
const manifests = array(index.manifests, 'OCI index manifests').map((entry, i) => object(entry, `manifest ${i}`));
if (manifests.length < 2) fail('published OCI index has too few manifests');
const ordinary = manifests.filter((entry) => object(entry.annotations ?? {}, 'descriptor annotations')['vnd.docker.reference.type'] !== 'attestation-manifest');
if (ordinary.length !== 1) fail('OCI index must contain exactly one subject manifest');
const subject = ordinary[0]!;
const platform = object(subject.platform, 'subject platform');
if (subject.mediaType !== 'application/vnd.oci.image.manifest.v1+json' || platform.os !== 'linux' || platform.architecture !== 'amd64') {
  fail('subject manifest must be linux/amd64 OCI');
}
const subjectDigest = digest(subject.digest, 'linux/amd64 subject digest');
const subjectHex = subjectDigest.slice('sha256:'.length);
const attestations = manifests.filter((entry) => object(entry.annotations ?? {}, 'descriptor annotations')['vnd.docker.reference.type'] === 'attestation-manifest');
if (attestations.length === 0) fail('OCI index contains no attestation manifests');
writeExclusive(indexPath, indexPayload);

let verifiedSpdx = false;
let verifiedSlsa = false;
for (const descriptor of attestations) {
  const annotations = object(descriptor.annotations ?? {}, 'attestation annotations');
  const referenced = digest(annotations['vnd.docker.reference.digest'], 'attestation subject annotation');
  if (referenced !== subjectDigest) fail('attestation descriptor references a different image manifest');
  const descriptorPlatform = object(descriptor.platform, 'attestation platform');
  if (descriptor.mediaType !== 'application/vnd.oci.image.manifest.v1+json' || descriptorPlatform.os !== 'unknown' || descriptorPlatform.architecture !== 'unknown') {
    fail('attestation descriptor must use the unknown/unknown OCI platform');
  }
  const attestationDigest = digest(descriptor.digest, 'attestation manifest digest');
  const manifestPayload = crane(['manifest', `${image}@${attestationDigest}`]);
  const manifest = parse(manifestPayload, 'attestation manifest');
  const manifestSubject = object(manifest.subject, 'attestation subject');
  if (manifest.schemaVersion !== 2 || manifest.mediaType !== 'application/vnd.oci.image.manifest.v1+json' ||
      manifest.artifactType !== 'application/vnd.docker.attestation.manifest.v1+json' ||
      manifestSubject.mediaType !== 'application/vnd.oci.image.manifest.v1+json' || manifestSubject.digest !== subjectDigest) {
    fail('attestation is not a native OCI artifact for the selected subject');
  }
  writeExclusive(join(evidenceDirectory, `attestation-manifest-${attestationDigest.slice(7)}.json`), manifestPayload);
  const layers = array(manifest.layers, 'attestation layers').map((entry, i) => object(entry, `layer ${i}`));
  const statements = layers.filter((layer) => layer.mediaType === 'application/vnd.in-toto+json');
  if (statements.length === 0) fail('attestation manifest has no in-toto layer');
  for (const layer of statements) {
    const layerDigest = digest(layer.digest, 'in-toto layer digest');
    const predicateType = text(object(layer.annotations ?? {}, 'layer annotations')['in-toto.io/predicate-type'], 'in-toto predicate type');
    if (!['https://spdx.dev/Document', 'https://slsa.dev/provenance/v1', 'https://slsa.dev/provenance/v0.2'].includes(predicateType)) fail('unrecognized in-toto predicate type');
    const statementPayload = crane(['blob', `${image}@${layerDigest}`]);
    const statement = parse(statementPayload, 'in-toto statement');
    if (!['https://in-toto.io/Statement/v0.1', 'https://in-toto.io/Statement/v1'].includes(text(statement._type, 'statement type')) || statement.predicateType !== predicateType) {
      fail('in-toto statement type or predicate does not match its descriptor');
    }
    const predicate = object(statement.predicate, 'statement predicate');
    if (predicateType === 'https://spdx.dev/Document') {
      if (predicate.SPDXID !== 'SPDXRef-DOCUMENT' || !text(predicate.spdxVersion, 'SPDX version').startsWith('SPDX-')) fail('SPDX predicate structure is invalid');
      verifiedSpdx = true;
    } else if (predicateType === 'https://slsa.dev/provenance/v1') {
      object(predicate.buildDefinition, 'SLSA buildDefinition'); object(predicate.runDetails, 'SLSA runDetails'); verifiedSlsa = true;
    } else {
      if (text(predicate.buildType, 'SLSA buildType').length === 0) fail('SLSA v0.2 buildType is empty');
      object(predicate.builder, 'SLSA builder'); verifiedSlsa = true;
    }
    const statementSubjects = array(statement.subject, 'statement subjects');
    if (statementSubjects.length === 0) fail('in-toto statement has no subjects');
    for (const [i, raw] of statementSubjects.entries()) {
      const item = object(raw, `statement subject ${i}`);
      const name = text(item.name, 'statement subject name');
      if (name !== image && !(name.startsWith(`pkg:docker/${image}@`) && name.endsWith('?platform=linux%2Famd64'))) fail('statement subject names a different image');
      const subjectHashes = object(item.digest, 'statement subject digest');
      if (Object.keys(subjectHashes).length !== 1 || subjectHashes.sha256 !== subjectHex) fail('statement subject digest is not exactly the selected sha256');
    }
    writeExclusive(join(evidenceDirectory, `in-toto-${layerDigest.slice(7)}.json`), statementPayload);
  }
}
if (!verifiedSpdx) fail('verified SPDX SBOM statement is missing');
if (!verifiedSlsa) fail('verified SLSA provenance statement is missing');
console.log(`BuildKit SBOM and provenance statements verified for ${image}@${indexDigest}`);
