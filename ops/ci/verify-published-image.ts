import { mkdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import {
  fail, parseObject, requireCanonicalDirectory, requireDigest, requireRevision, run, writeExclusive,
  type JsonObject,
} from './release-evidence.ts';

const SCOPE = 'published image verification';
const [image = '', digestValue = '', cacheScope = '', runnerTemporary = ''] = process.argv.slice(2);
const revision = requireRevision(process.env.GITHUB_SHA, SCOPE);
if (process.env.GITHUB_REPOSITORY !== 'memeloop-online/memeloop-token-center') fail(SCOPE, 'unexpected GitHub repository');
const owner = process.env.GITHUB_REPOSITORY_OWNER;
if (owner === undefined || !/^[a-z0-9-]+$/.test(owner)) fail(SCOPE, 'repository owner is invalid');
const server = process.env.GITHUB_SERVER_URL;
if (server === undefined || !/^https:\/\/[^/]+$/.test(server)) fail(SCOPE, 'GitHub server URL is invalid');
const expectedNames: Record<string, string> = {
  service: 'memeloop-token-center',
  importer: 'memeloop-token-center-importer',
  'plugin-installer': 'memeloop-token-center-plugin-installer',
};
const name = expectedNames[cacheScope];
if (name === undefined) fail(SCOPE, 'cache scope is invalid');
const expectedImage = `ghcr.io/${owner}/${name}`;
if (image !== expectedImage) fail(SCOPE, `image must be ${expectedImage}`);
const digest = requireDigest(digestValue, SCOPE, 'published image digest');
const runner = requireCanonicalDirectory(runnerTemporary, SCOPE, 'runner temporary directory');
const tag = `sha-${revision}`;
const taggedReference = `${image}:${tag}`;
const resolved = run('crane', ['digest', taggedReference], SCOPE, 'immutable tag resolution').trim();
if (resolved !== digest) fail(SCOPE, 'immutable tag does not resolve to the build digest');

const indexPath = join(runner, `${cacheScope}-index.json`);
const attestations = join(runner, `${cacheScope}-attestations`);
mkdirSync(attestations, { mode: 0o700 });
run(process.execPath, [
  'ops/ci/verify-buildkit-attestations.ts', image, digest, indexPath, attestations,
], SCOPE, 'BuildKit attestation verification');

const index = parseObject(readFileSync(indexPath, 'utf8'), SCOPE, 'OCI index evidence');
if (!Array.isArray(index.manifests)) fail(SCOPE, 'OCI index manifests are missing');
const subjects = index.manifests.filter((entry): entry is JsonObject => {
  if (entry === null || Array.isArray(entry) || typeof entry !== 'object') return false;
  const annotations = entry.annotations;
  return annotations === null || Array.isArray(annotations) || typeof annotations !== 'object' ||
    (annotations as JsonObject)['vnd.docker.reference.type'] !== 'attestation-manifest';
});
if (subjects.length !== 1 || typeof subjects[0]!.digest !== 'string') fail(SCOPE, 'OCI index must expose exactly one platform subject');
const platformDigest = requireDigest(subjects[0]!.digest, SCOPE, 'platform digest');
const imagePayload = run('docker', [
  'buildx', 'imagetools', 'inspect', `${image}@${platformDigest}`, '--format', '{{json .Image}}',
], SCOPE, 'immutable platform inspection');
const imageEvidence = parseObject(imagePayload, SCOPE, 'platform image evidence');
const config = imageEvidence.config;
if (config === null || Array.isArray(config) || typeof config !== 'object') fail(SCOPE, 'platform config is missing');
const labels = (config as JsonObject).Labels;
if (labels === null || Array.isArray(labels) || typeof labels !== 'object') fail(SCOPE, 'platform labels are missing');
if ((labels as JsonObject)['org.opencontainers.image.source'] !== `${server}/${process.env.GITHUB_REPOSITORY}` ||
    (labels as JsonObject)['org.opencontainers.image.revision'] !== revision) {
  fail(SCOPE, 'platform source or revision label does not match the release');
}
writeExclusive(join(runner, `${cacheScope}-image.json`), `${JSON.stringify(imageEvidence)}\n`, SCOPE);
const manifest = { schema_version: 1, image, tag, digest, revision, reference: `${image}@${digest}` };
writeExclusive(join(runner, `${cacheScope}-image-digest.json`), `${JSON.stringify(manifest)}\n`, SCOPE);
console.log(`Verified immutable image evidence for ${manifest.reference}`);
