import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { readFileSync, readdirSync } from 'node:fs';
import { relative, resolve } from 'node:path';
import test from 'node:test';
import { contains, excludes, repository, run } from './contract-helpers.ts';

const vendor = resolve(repository, 'vendor/rust_decimal');
const sha256 = (payload: Buffer | string): string => createHash('sha256').update(payload).digest('hex');
const fileSha = (path: string): string => sha256(readFileSync(path));

function files(root: string): string[] {
  return readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const path = resolve(root, entry.name);
    return entry.isDirectory() ? files(path) : entry.isFile() ? [path] : [];
  }).sort((a, b) => Buffer.from(a).compare(Buffer.from(b)));
}

function treeDigest(paths: string[]): string {
  return sha256(paths.map((path) => `${fileSha(path)}  ${relative(repository, path)}\n`).join(''));
}

test('vendored rust_decimal fork matches reviewed upstream release', () => {
  assert.equal(fileSha(resolve(vendor, 'Cargo.toml')), '153bad3625511c60b3f6d2fccf5da952063a11efe950e781c287e3fb52324387');
  assert.equal(fileSha(resolve(vendor, 'MEMELOOP-MANIFEST.patch')), '0c2826a356f71e39f57c79695de8d332dba8d339ce9b5ad8a5a8ea45c8ca2549');
  const reversed = spawnSync('patch', ['--reverse', '--silent', '--output=-', resolve(vendor, 'Cargo.toml')], {
    cwd: repository, encoding: 'buffer', input: readFileSync(resolve(vendor, 'MEMELOOP-MANIFEST.patch')), shell: false,
  });
  assert.equal(reversed.status, 0, 'manifest patch could not be reversed');
  assert.equal(sha256(reversed.stdout), '33cbd9b506cfaa14d1df68bab1af011ceccbd66000a6d68a5ef56ecb776ac9ac');
  const ignored = new Set(['Cargo.toml', 'Cargo.lock', 'MEMELOOP-FORK.md', 'MEMELOOP-MANIFEST.patch']);
  assert.equal(treeDigest(files(vendor).filter((path) => !ignored.has(relative(vendor, path)))), 'bcc9fba4b64831a9db0aab840e7f0fff25aa70e5529a238f0ff8709cf1b34f4f');
  assert.equal(treeDigest(files(resolve(vendor, 'src'))), 'bed83f744adbfb12b004dfd3d4157286bae3e5762bed8d872df99f44e3bcd2f7');
  contains('vendor/rust_decimal/.cargo_vcs_info.json', '"sha1": "c7efe1690bd8e460731ff97a7c4941ecffc8751b"');
  contains('vendor/rust_decimal/Cargo.toml.orig', 'rkyv = { default-features = false');
  excludes('vendor/rust_decimal/Cargo.toml', /^(?:rkyv|rkyv-safe) =|^\[dependencies\.rkyv\]|dev-dependencies\.rkyv/m);
  excludes('Cargo.lock', 'name = "rkyv"');
  assert.throws(() => run('git', ['ls-files', '--error-unmatch', 'vendor/rust_decimal/Cargo.lock']));
  contains('vendor/rust_decimal/MEMELOOP-FORK.md', 'be2a24f50780bc85f09cc6ac299bdf1424302742d77221106859c9d8b102126a');
});
