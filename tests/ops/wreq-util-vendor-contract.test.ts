import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { cpSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { relative, resolve } from 'node:path';
import test from 'node:test';
import { repository } from './contract-helpers.ts';

const vendor = resolve(repository, 'vendor/wreq-util');
const archiveChecksum = '1d8d73c75be86fbde675988dbe5cb15f02567025c13f6ffab910361ad1ba6c89';
const digest = (data: Buffer | string): string => createHash('sha256').update(data).digest('hex');
function inventory(root: string): Record<string, string> {
  const result: Record<string, string> = {};
  function visit(directory: string): void {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const path = resolve(directory, entry.name);
      if (entry.isDirectory()) visit(path);
      else {
        assert(entry.isFile(), 'no symlinks in reviewed crate');
        result[relative(root, path)] = digest(readFileSync(path));
      }
    }
  }
  visit(root);
  return result;
}

function lockEntry(lock: string, name: string): string {
  const entries = lock.split('[[package]]').filter((entry) => entry.startsWith(`\nname = "${name}"\n`));
  assert.equal(entries.length, 1, `exactly one selected ${name} package`);
  return entries[0]!;
}

test('wreq-util is the authenticated release with only its reviewed zstd constraint updated', async () => {
  const response = await fetch('https://static.crates.io/crates/wreq-util/wreq-util-3.0.0-rc.14.crate', {
    signal: AbortSignal.timeout(30_000),
  });
  assert(response.ok);
  const archive = Buffer.from(await response.arrayBuffer());
  assert.equal(digest(archive), archiveChecksum, 'official registry checksum mismatch');
  const root = mkdtempSync(resolve(tmpdir(), 'mtc-wreq-util-provenance-'));
  try {
    const extract = spawnSync('tar', ['-xzf', '-', '-C', root], { input: archive });
    assert.equal(extract.status, 0);
    const upstream = inventory(resolve(root, 'wreq-util-3.0.0-rc.14'));
    assert.equal(Object.keys(upstream).length, 55);
    assert.deepEqual(JSON.parse(readFileSync(resolve(vendor, 'UPSTREAM-SHA256.json'), 'utf8')), upstream);
    const patch = readFileSync(resolve(vendor, 'MEMELOOP-MANIFEST.patch'), 'utf8');
    assert.equal(digest(patch), '5c7aa0e8271324feee098154a9a628ef4312dd83c236f459d377ba821a1faebc');
    assert.deepEqual([...patch.matchAll(/^\+\+\+ b\/(.+)$/gm)].map((match) => match[1]), ['Cargo.toml']);
    assert.equal(digest(readFileSync(resolve(vendor, 'Cargo.toml'))), '84d4ab08cfca091c9147755535763b7793bfb888ff7d05acfcc46458b97a4e61');
    const patched = resolve(root, 'patched');
    cpSync(vendor, patched, { recursive: true });
    for (const extra of ['MEMELOOP-FORK.md', 'MEMELOOP-MANIFEST.patch', 'UPSTREAM-SHA256.json']) {
      rmSync(resolve(patched, extra));
    }
    const reversed = spawnSync('patch', ['-p1', '--reverse', '--batch', '--fuzz=0'], {
      cwd: patched, input: patch, encoding: 'utf8',
    });
    assert.equal(reversed.status, 0, reversed.stdout + reversed.stderr);
    assert.deepEqual(inventory(patched), upstream, 'unreviewed upstream source or metadata difference');
    const cargo = readFileSync(resolve(repository, 'Cargo.toml'), 'utf8');
    assert.match(cargo, /^zstd = "0\.14\.3"$/m);
    assert.match(cargo.split('[patch.crates-io]')[1]!, /^wreq-util = \{ path = "vendor\/wreq-util" \}$/m);
    const lock = readFileSync(resolve(repository, 'Cargo.lock'), 'utf8');
    const util = lockEntry(lock, 'wreq-util');
    assert.match(util, /\nversion = "3\.0\.0-rc\.14"\n/);
    assert(!util.includes('source =') && !util.includes('checksum ='));
    for (const [name, version, checksum] of [
      ['zstd', '0.14.3', 'db3ae9bf90af6e40197bef767eeefc46ad6397420fe873830bd85e5fbbfb3c70'],
      ['zstd-safe', '8.2.0', 'ded520f16b29e1a9b771c722221b2b0a0398c2316d9bb3c42b458a5414d7434b'],
      ['zstd-sys', '2.1.2+zstd.1.5.7', '481ada9c543b2323cbb7bbdbb5249177fbf0188ed959ed2c15e4b22421c84ed3'],
    ]) {
      const entry = lockEntry(lock, name!);
      assert(entry.includes(`\nversion = "${version}"\n`));
      assert(entry.includes('\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n'));
      assert(entry.includes(`\nchecksum = "${checksum}"\n`));
    }
    const docker = readFileSync(resolve(repository, 'Dockerfile'), 'utf8');
    assert(docker.includes('COPY vendor/wreq-util/LICENSE /release-input/third-party-licenses/wreq-util-LICENSE'));
    const notices = readFileSync(resolve(repository, 'THIRD_PARTY_NOTICES.md'), 'utf8');
    assert(notices.includes('`wreq-util` 3.0.0-rc.14'));
    assert(notices.includes('`third-party-licenses/wreq-util-LICENSE`'));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
