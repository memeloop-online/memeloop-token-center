import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { readFileSync, readdirSync } from 'node:fs';
import { relative, resolve } from 'node:path';
import test from 'node:test';
import { repository } from './contract-helpers.ts';

const vendor = resolve(repository, 'vendor/wreq-util');
const hash = (value: Buffer | string) => createHash('sha256').update(value).digest('hex');
const fileHash = (path: string) => hash(readFileSync(path));
const files = (directory: string): string[] => readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
  const path = resolve(directory, entry.name);
  return entry.isDirectory() ? files(path) : [relative(vendor, path)];
}).sort();

test('wreq-util changes only the authenticated upstream zstd dependency requirement', () => {
  const inventoryPath = resolve(vendor, 'UPSTREAM-SHA256.json');
  assert.equal(fileHash(inventoryPath), '48a294ce3d37a38db41dbb54dd5b6970937cda430faf065f8a1cc263a9e30b43');
  const inventory = JSON.parse(readFileSync(inventoryPath, 'utf8')) as Record<string, string>;
  const localFiles = ['Cargo.toml', 'MEMELOOP-FORK.md', 'MEMELOOP-MANIFEST.patch', 'UPSTREAM-SHA256.json'];
  assert.deepEqual(files(vendor), [...Object.keys(inventory), ...localFiles].sort(), 'all runtime files must be accounted for');
  for (const [path, digest] of Object.entries(inventory)) assert.equal(fileHash(resolve(vendor, path)), digest, path);
  const manifestPath = resolve(vendor, 'Cargo.toml');
  assert.equal(fileHash(manifestPath), '84d4ab08cfca091c9147755535763b7793bfb888ff7d05acfcc46458b97a4e61');
  const patchPath = resolve(vendor, 'MEMELOOP-MANIFEST.patch');
  assert.equal(fileHash(patchPath), '5c7aa0e8271324feee098154a9a628ef4312dd83c236f459d377ba821a1faebc');
  const reversed = spawnSync('patch', ['--reverse', '--fuzz=0', '--silent', '--output=-', manifestPath], {
    cwd: repository, input: readFileSync(patchPath), shell: false,
  });
  assert.equal(reversed.status, 0);
  assert.equal(hash(reversed.stdout), 'b82f56a15f08d2de97f80bd043d41ee0711bf00dabf3a9d36ebdb492b6c8de42');
});
