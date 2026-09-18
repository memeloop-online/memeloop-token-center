import { createHash } from 'node:crypto';
import { lstatSync, readFileSync, readdirSync, realpathSync, statSync } from 'node:fs';
import { join, relative, resolve, sep } from 'node:path';
import { fail, parseObject, requireCanonicalDirectory, requireRevision } from './release-evidence.ts';

const SCOPE = 'release service input verification';
const [directoryValue = '', revisionValue = ''] = process.argv.slice(2);
if (directoryValue === '' || revisionValue === '') fail(SCOPE, 'directory and revision are required');
const directory = requireCanonicalDirectory(resolve(directoryValue), SCOPE, 'release input directory');
const revision = requireRevision(revisionValue, SCOPE);
const manifest = parseObject(readFileSync(join(directory, 'release-service-input.json'), 'utf8'), SCOPE, 'release input manifest');
if (
  manifest.schema_version !== 2
  || manifest.revision !== revision
  || manifest.platform !== 'linux/amd64'
  || !Array.isArray(manifest.roles)
  || JSON.stringify(manifest.roles) !== JSON.stringify(['service', 'plugin-installer'])
  || !Array.isArray(manifest.features)
  || manifest.features.length !== 2
  || manifest.features[0] !== 'experimental-plugin-revisions'
  || manifest.features[1] !== 'plugin-distribution'
  || manifest.files === null
  || Array.isArray(manifest.files)
  || typeof manifest.files !== 'object'
) fail(SCOPE, 'release input manifest identity is invalid');
const expected = manifest.files as Record<string, unknown>;
const files: string[] = [];
const visit = (parent: string): void => {
  for (const entry of readdirSync(parent, { withFileTypes: true }).sort((left, right) => left.name.localeCompare(right.name, 'en'))) {
    const path = join(parent, entry.name);
    const metadata = lstatSync(path);
    if (metadata.isSymbolicLink() || realpathSync(path) !== path) fail(SCOPE, 'release input must not contain symlinks or path aliases');
    if (entry.isDirectory()) visit(path);
    else if (entry.isFile()) {
      const name = relative(directory, path).split(sep).join('/');
      if (name !== 'release-service-input.json') files.push(name);
    } else fail(SCOPE, `${entry.name} must be a regular file or directory`);
  }
};
visit(directory);
if (JSON.stringify(Object.keys(expected).sort()) !== JSON.stringify(files.sort())) {
  fail(SCOPE, 'release input file set does not match the sealed manifest');
}
for (const name of files) {
  const path = join(directory, name);
  const metadata = lstatSync(path);
  if (!metadata.isFile() || metadata.isSymbolicLink() || realpathSync(path) !== path || statSync(path).size === 0) {
    fail(SCOPE, `${name} must be a canonical non-empty regular file`);
  }
  const digest = createHash('sha256').update(readFileSync(path)).digest('hex');
  if (expected[name] !== digest) fail(SCOPE, `${name} digest does not match the manifest`);
}
console.log(`Verified Docker-native service release input for ${revision}`);
