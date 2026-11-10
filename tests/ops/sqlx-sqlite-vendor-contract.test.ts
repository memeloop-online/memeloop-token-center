import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { relative, resolve } from 'node:path';
import test from 'node:test';
import { contains, repository } from './contract-helpers.ts';

const vendor = resolve(repository, 'vendor/sqlx-sqlite');
const sha256 = (payload: Buffer | string): string => createHash('sha256').update(payload).digest('hex');
const fileSha = (path: string): string => sha256(readFileSync(path));
function files(root: string): string[] {
  return readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const path = resolve(root, entry.name);
    assert.ok(!entry.isSymbolicLink(), 'vendor inventory must contain regular files');
    return entry.isDirectory() ? files(path) : entry.isFile() ? [relative(vendor, path)] : [];
  }).sort();
}

test('SQLx SQLite fork preserves authenticated release sources and licenses', () => {
  const metadata = resolve(vendor, 'MEMELOOP-UPSTREAM.json');
  assert.equal(fileSha(metadata), '920ddb21230af1e1f718d9bd78bf7093b1a8b84c06b61874e0ef89878a881cc4');
  const upstream = JSON.parse(readFileSync(metadata, 'utf8')) as {
    crate: string; version: string; archive_sha256: string;
    files: Record<string, string>; omitted: Record<string, string>;
  };
  assert.equal(upstream.crate, 'sqlx-sqlite');
  assert.equal(upstream.version, '0.9.0');
  assert.equal(upstream.archive_sha256, '488e99c397a62007e4229aec669a179816339afc6d2620ca6fa420dbee2e982c');
  assert.equal(Object.keys(upstream.files).length, 57);
  assert.deepEqual(Object.keys(upstream.omitted), ['Cargo.lock']);
  assert.match(upstream.files['Cargo.lock'], /^[0-9a-f]{64}$/);
  assert.equal(existsSync(resolve(vendor, 'Cargo.lock')), false);
  const auditFiles = ['MEMELOOP-FORK.md', 'MEMELOOP-MANIFEST.patch', 'MEMELOOP-UPSTREAM.json'];
  assert.deepEqual(files(vendor), [...Object.keys(upstream.files).filter(path => path !== 'Cargo.lock'), ...auditFiles].sort());
  for (const [path, digest] of Object.entries(upstream.files)) {
    if (path !== 'Cargo.lock' && path !== 'Cargo.toml') assert.equal(fileSha(resolve(vendor, path)), digest, path);
  }
  assert.equal(fileSha(resolve(vendor, 'Cargo.toml')), '493aa99eb2ebe598420db7dcd523ba8ce61f2d0f0c0439cb200baf8f1b73a2aa');
  assert.equal(fileSha(resolve(vendor, 'MEMELOOP-MANIFEST.patch')), '7e8055ee2058bc909a40bf335e998d91876260e03e1e7d97fe64b2063628ceb7');
  const reversed = spawnSync('patch', ['--reverse', '--silent', '--output=-', resolve(vendor, 'Cargo.toml')], {
    cwd: repository, encoding: 'buffer', input: readFileSync(resolve(vendor, 'MEMELOOP-MANIFEST.patch')), shell: false,
  });
  assert.equal(reversed.status, 0, 'manifest patch must reverse cleanly');
  assert.equal(sha256(reversed.stdout), upstream.files['Cargo.toml']);
  contains('vendor/sqlx-sqlite/Cargo.toml', 'version = ">=0.39.1, <0.40.0"');
  contains('vendor/sqlx-sqlite/MEMELOOP-FORK.md', upstream.archive_sha256);
  contains('vendor/sqlx-sqlite/MEMELOOP-FORK.md', '10462fa4414c6c7f1ed1d39bb31678e3b726878c');
});

test('workspace locks the patched SQLite bundle and retains its existing build closure', () => {
  const packages = readFileSync(resolve(repository, 'Cargo.lock'), 'utf8').split('[[package]]');
  const record = (name: string) => {
    const matches = packages.filter(p => p.includes(`\nname = "${name}"\n`));
    assert.equal(matches.length, 1, `one ${name} package`);
    return matches[0];
  };
  const sqlite = record('libsqlite3-sys');
  assert.match(sqlite, /\nversion = "0\.39\.1"\n/);
  assert.match(sqlite, /checksum = "14a5f01b87b92705d07b433c06f96cbc764637d81a0bffb5734ce17cf0f9a2b3"/);
  assert.match(sqlite, /source = "registry\+https:\/\/github.com\/rust-lang\/crates.io-index"/);
  assert.match(sqlite, /dependencies = \[\n "cc",\n "pkg-config",\n "vcpkg",\n\]/);
  const sqlx = record('sqlx-sqlite');
  assert.match(sqlx, /\nversion = "0\.9\.0"\n/);
  assert.doesNotMatch(sqlx, /\n(?:source|checksum) =/);
  contains('Cargo.toml', 'sqlx-sqlite = { path = "vendor/sqlx-sqlite" }');
});
