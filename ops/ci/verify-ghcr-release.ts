import { lstatSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import {
  fail, parseObject, requireCanonicalDirectory, requireDigest, requireRevision, run, writeExclusive,
  type JsonObject,
} from './release-evidence.ts';

const SCOPE = 'GHCR release verification';
const [evidenceValue = '', output = ''] = process.argv.slice(2);
const revision = requireRevision(process.env.GITHUB_SHA, SCOPE);
if (process.env.GITHUB_REPOSITORY !== 'memeloop-online/memeloop-token-center') fail(SCOPE, 'unexpected GitHub repository');
const owner = process.env.GITHUB_REPOSITORY_OWNER;
if (owner === undefined || !/^[a-z0-9-]+$/.test(owner)) fail(SCOPE, 'repository owner is invalid');
const evidence = requireCanonicalDirectory(evidenceValue, SCOPE, 'release evidence directory');
if (output === '') fail(SCOPE, 'release manifest output is required');
const expected: Record<string, string> = {
  'service-image-digest.json': `ghcr.io/${owner}/memeloop-token-center`,
  'importer-image-digest.json': `ghcr.io/${owner}/memeloop-token-center-importer`,
  'plugin-installer-image-digest.json': `ghcr.io/${owner}/memeloop-token-center-plugin-installer`,
};
const names = readdirSync(evidence).filter((name) => name.endsWith('-image-digest.json')).sort();
if (JSON.stringify(names) !== JSON.stringify(Object.keys(expected).sort())) fail(SCOPE, 'release evidence set is incomplete or contains unexpected images');
const records: JsonObject[] = [];
for (const name of names) {
  const path = join(evidence, name);
  const metadata = lstatSync(path);
  if (!metadata.isFile() || metadata.isSymbolicLink() || (metadata.mode & 0o022) !== 0) fail(SCOPE, `${name} must be a non-writable regular file`);
  const record = parseObject(readFileSync(path, 'utf8'), SCOPE, name);
  const keys = Object.keys(record).sort();
  if (JSON.stringify(keys) !== JSON.stringify(['digest','image','reference','revision','schema_version','tag'].sort())) fail(SCOPE, `${name} fields are invalid`);
  const image = expected[name]!;
  if (record.schema_version !== 1 || record.image !== image || record.revision !== revision || record.tag !== `sha-${revision}`) fail(SCOPE, `${name} identity does not match the release`);
  const digest = requireDigest(typeof record.digest === 'string' ? record.digest : '', SCOPE, `${name} digest`);
  if (record.reference !== `${image}@${digest}`) fail(SCOPE, `${name} immutable reference is invalid`);
  records.push(record);
}
records.sort((left, right) => String(left.image).localeCompare(String(right.image), 'en'));
for (const record of records) run('docker', ['buildx', 'imagetools', 'inspect', String(record.reference)], SCOPE, 'remote immutable reference inspection');
writeExclusive(output, `${JSON.stringify(records)}\n`, SCOPE);
console.log(`Verified ${records.length} immutable GHCR release images for ${revision}`);
