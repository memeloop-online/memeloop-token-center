import { createHash } from 'node:crypto';
import {
  constants,
  copyFileSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  writeFileSync,
} from 'node:fs';
import { basename, join, resolve } from 'node:path';
import { fail, requireRevision } from './release-evidence.ts';

const SCOPE = 'memory binary creation';
const [binaryValue = '', revisionValue = '', outputValue = ''] = process.argv.slice(2);
const revision = requireRevision(revisionValue, SCOPE);
const binary = resolve(binaryValue);
const output = resolve(outputValue);
if (binaryValue === '' || outputValue === '') fail(SCOPE, 'binary and output directory are required');
const metadata = lstatSync(binary);
if (!metadata.isFile() || metadata.isSymbolicLink() || realpathSync(binary) !== binary) {
  fail(SCOPE, 'binary must be a canonical non-symlink regular file');
}
mkdirSync(output, { recursive: false, mode: 0o700 });
const name = 'memeloop-token-center';
const destination = join(output, name);
copyFileSync(binary, destination, constants.COPYFILE_EXCL);
const sha256 = createHash('sha256').update(readFileSync(destination)).digest('hex');
writeFileSync(
  join(output, 'memory-binary.json'),
  `${JSON.stringify({ schema_version: 1, name, revision, sha256, source_name: basename(binary) })}\n`,
  { encoding: 'utf8', flag: 'wx', mode: 0o600 },
);
console.log(`Sealed memory acceptance binary ${sha256} for ${revision}`);
