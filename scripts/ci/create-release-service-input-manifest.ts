import { createHash } from 'node:crypto';
import { lstatSync, readFileSync, readdirSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import { join, relative, resolve, sep } from 'node:path';
import { fail, requireCanonicalDirectory, requireRevision } from './release-evidence.ts';

const SCOPE = 'release service input creation';
const [directoryValue = '', revisionValue = ''] = process.argv.slice(2);
if (directoryValue === '' || revisionValue === '') fail(SCOPE, 'directory and revision are required');
const directory = requireCanonicalDirectory(resolve(directoryValue), SCOPE, 'release input directory');
const revision = requireRevision(revisionValue, SCOPE);
const required = [
  'LICENSE',
  'THIRD_PARTY_NOTICES.md',
  'cosign',
  'install-plugin-oci',
  'libgcc_s.so.1',
  'libstdc++.so.6',
  'memeloop-token-center',
  'memory-binary.json',
  'third-party-licenses/cosign-LICENSE',
  'third-party-licenses/rust_decimal-LICENSE',
  'web/index.html',
] as const;
const paths: string[] = [];
const visit = (parent: string): void => {
  for (const entry of readdirSync(parent, { withFileTypes: true }).sort((left, right) => left.name.localeCompare(right.name, 'en'))) {
    const path = join(parent, entry.name);
    const metadata = lstatSync(path);
    if (metadata.isSymbolicLink() || realpathSync(path) !== path) fail(SCOPE, 'release input must not contain symlinks or path aliases');
    if (entry.isDirectory()) visit(path);
    else if (entry.isFile()) {
      const name = relative(directory, path).split(sep).join('/');
      if (name !== 'release-service-input.json') paths.push(name);
    } else fail(SCOPE, `${entry.name} must be a regular file or directory`);
  }
};
visit(directory);
for (const name of required) if (!paths.includes(name)) fail(SCOPE, `${name} is missing from the complete release input`);
const digests: Record<string, string> = {};
for (const name of paths) {
  const path = join(directory, name);
  const metadata = lstatSync(path);
  if (!metadata.isFile() || metadata.isSymbolicLink() || realpathSync(path) !== path || statSync(path).size === 0) {
    fail(SCOPE, `${name} must be a canonical non-empty regular file`);
  }
  if (['memeloop-token-center', 'install-plugin-oci', 'cosign'].includes(name) && (metadata.mode & 0o111) === 0) {
    fail(SCOPE, `${name} must be executable before artifact transport`);
  }
  digests[name] = createHash('sha256').update(readFileSync(path)).digest('hex');
}
writeFileSync(
  join(directory, 'release-service-input.json'),
  `${JSON.stringify({
    schema_version: 2,
    revision,
    platform: 'linux/amd64',
    roles: ['service', 'plugin-installer'],
    features: ['experimental-plugin-revisions', 'plugin-distribution'],
    files: digests,
  })}\n`,
  { encoding: 'utf8', flag: 'wx', mode: 0o600 },
);
console.log(`Sealed Docker-native service release input for ${revision}`);
