import { spawnSync } from 'node:child_process';
import { existsSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

function fail(message: string): never {
  throw new Error(`release input validation: ${message}`);
}

const repository = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const [registry = '', revision = '', tagStyle = 'exact'] = process.argv.slice(2);
if (!/^[a-z0-9][a-z0-9._/-]*$/.test(registry) || registry.startsWith('/') || registry.endsWith('/') || registry.includes('//') || registry.includes('..')) {
  fail('registry prefix must be a lowercase OCI host/path without a scheme');
}
if (!/^[0-9a-f]{40}$/.test(revision)) fail('revision must contain exactly 40 lowercase hexadecimal characters');
if (tagStyle !== 'exact' && tagStyle !== 'prefixed') fail('tag style must be exact or prefixed');

const git = (args: string[]): string => {
  const result = spawnSync('git', args, { cwd: repository, encoding: 'utf8', shell: false });
  if (result.status !== 0) fail(result.stderr.trim() || `git ${args.join(' ')} failed`);
  return result.stdout.trim();
};
const resolved = git(['rev-parse', 'HEAD']);
if (resolved !== revision) fail(`checkout is ${resolved}, expected ${revision}`);
if (git(['status', '--porcelain=v1', '--untracked-files=all']) !== '') fail('release checkout contains tracked or untracked changes');
for (const path of ['Dockerfile', 'Dockerfile.importer', 'Dockerfile.plugin-installer']) {
  if (!existsSync(resolve(repository, path))) fail(`${path} is missing`);
}
// Force a directory read so a concurrently removed checkout fails before any output is trusted.
readdirSync(repository);
const tag = tagStyle === 'exact' ? revision : `sha-${revision}`;
for (const [kind, dockerfile, name] of [
  ['service', 'Dockerfile', 'memeloop-token-center'],
  ['importer', 'Dockerfile.importer', 'memeloop-token-center-importer'],
  ['plugin-installer', 'Dockerfile.plugin-installer', 'memeloop-token-center-plugin-installer'],
]) console.log(`${kind}|${dockerfile}|${name}|${registry}/${name}:${tag}`);
