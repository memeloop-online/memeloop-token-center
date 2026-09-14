import { createHash } from 'node:crypto';
import { lstatSync, readFileSync, realpathSync, statSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fail, parseObject, requireCanonicalDirectory, requireRevision } from './release-evidence.ts';

const SCOPE = 'release service input verification';
const [directoryValue = '', revisionValue = ''] = process.argv.slice(2);
if (directoryValue === '' || revisionValue === '') fail(SCOPE, 'directory and revision are required');
const directory = requireCanonicalDirectory(resolve(directoryValue), SCOPE, 'release input directory');
const revision = requireRevision(revisionValue, SCOPE);
const manifest = parseObject(readFileSync(join(directory, 'release-service-input.json'), 'utf8'), SCOPE, 'release input manifest');
const files = ['memeloop-token-center', 'install-plugin-oci', 'cosign', 'libgcc_s.so.1', 'libstdc++.so.6'] as const;
if (
  manifest.schema_version !== 1
  || manifest.revision !== revision
  || manifest.platform !== 'linux/amd64'
  || !Array.isArray(manifest.features)
  || manifest.features.length !== 2
  || manifest.features[0] !== 'experimental-plugin-revisions'
  || manifest.features[1] !== 'plugin-distribution'
  || manifest.files === null
  || Array.isArray(manifest.files)
  || typeof manifest.files !== 'object'
) fail(SCOPE, 'release input manifest identity is invalid');
const expected = manifest.files as Record<string, unknown>;
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
