import { createHash } from 'node:crypto';
import { lstatSync, readFileSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fail, requireCanonicalDirectory, requireRevision } from './release-evidence.ts';

const SCOPE = 'release service input creation';
const [directoryValue = '', revisionValue = ''] = process.argv.slice(2);
if (directoryValue === '' || revisionValue === '') fail(SCOPE, 'directory and revision are required');
const directory = requireCanonicalDirectory(resolve(directoryValue), SCOPE, 'release input directory');
const revision = requireRevision(revisionValue, SCOPE);
const files = ['memeloop-token-center', 'libgcc_s.so.1', 'libstdc++.so.6'] as const;
const digests: Record<string, string> = {};
for (const name of files) {
  const path = join(directory, name);
  const metadata = lstatSync(path);
  if (!metadata.isFile() || metadata.isSymbolicLink() || realpathSync(path) !== path || statSync(path).size === 0) {
    fail(SCOPE, `${name} must be a canonical non-empty regular file`);
  }
  if (name === 'memeloop-token-center' && (metadata.mode & 0o111) === 0) fail(SCOPE, 'service binary must be executable');
  digests[name] = createHash('sha256').update(readFileSync(path)).digest('hex');
}
writeFileSync(
  join(directory, 'release-service-input.json'),
  `${JSON.stringify({ schema_version: 1, revision, platform: 'linux/amd64', features: ['experimental-plugin-revisions'], files: digests })}\n`,
  { encoding: 'utf8', flag: 'wx', mode: 0o600 },
);
console.log(`Sealed Docker-native service release input for ${revision}`);
