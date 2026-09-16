import { createHash } from 'node:crypto';
import {
  chmodSync,
  constants,
  copyFileSync,
  lstatSync,
  readFileSync,
  realpathSync,
} from 'node:fs';
import { join, resolve } from 'node:path';
import {
  fail,
  parseObject,
  requireCanonicalDirectory,
  requireRevision,
} from './release-evidence.ts';

const SCOPE = 'memory binary installation';
const [artifactValue = '', revisionValue = '', destinationValue = ''] = process.argv.slice(2);
if (artifactValue === '' || revisionValue === '' || destinationValue === '') {
  fail(SCOPE, 'artifact directory, revision, and destination are required');
}
const artifact = requireCanonicalDirectory(resolve(artifactValue), SCOPE, 'artifact directory');
const revision = requireRevision(revisionValue, SCOPE);
const manifest = parseObject(
  readFileSync(join(artifact, 'memory-binary.json'), 'utf8'),
  SCOPE,
  'binary manifest',
);
if (
  manifest.schema_version !== 1
  || manifest.name !== 'memeloop-token-center'
  || manifest.revision !== revision
  || typeof manifest.sha256 !== 'string'
  || !/^[0-9a-f]{64}$/.test(manifest.sha256)
) {
  fail(SCOPE, 'binary manifest identity is invalid');
}
const source = join(artifact, 'memeloop-token-center');
const metadata = lstatSync(source);
if (!metadata.isFile() || metadata.isSymbolicLink() || realpathSync(source) !== source) {
  fail(SCOPE, 'artifact binary must be a canonical non-symlink regular file');
}
const actual = createHash('sha256').update(readFileSync(source)).digest('hex');
if (actual !== manifest.sha256) fail(SCOPE, 'artifact binary digest does not match its manifest');
const destination = resolve(destinationValue);
copyFileSync(source, destination, constants.COPYFILE_EXCL);
chmodSync(destination, 0o500);
console.log(`Installed verified memory acceptance binary ${actual} for ${revision}`);
