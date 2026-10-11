import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { cpSync, mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve, relative } from 'node:path';
import test from 'node:test';
import { repository } from './contract-helpers.ts';

const vendor = resolve(repository, 'vendor/wreq-proto');
const checksum = 'cb4d829f0427a5064829976fe790641ca21abb1d83d844812baf022dc0bdf2db';
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

test('wreq-proto source is authenticated upstream plus only reviewed binding patch', async () => {
  const response = await fetch('https://static.crates.io/crates/wreq-proto/wreq-proto-0.2.6.crate', {
    signal: AbortSignal.timeout(30_000),
  });
  assert(response.ok);
  const archive = Buffer.from(await response.arrayBuffer());
  assert.equal(digest(archive), checksum, 'official registry checksum mismatch');
  const root = mkdtempSync(resolve(tmpdir(), 'mtc-wreq-provenance-'));
  try {
    const extract = spawnSync('tar', ['-xzf', '-', '-C', root], { input: archive });
    assert.equal(extract.status, 0);
    const upstream = inventory(resolve(root, 'wreq-proto-0.2.6'));
    assert.deepEqual(JSON.parse(readFileSync(resolve(vendor, 'UPSTREAM-SHA256.json'), 'utf8')), upstream);
    const patched = resolve(root, 'patched');
    cpSync(vendor, patched, { recursive: true });
    for (const extra of ['MEMELOOP-FORK.md', 'MEMELOOP-SOURCE.patch', 'UPSTREAM-SHA256.json']) {
      rmSync(resolve(patched, extra));
    }
    const patch = readFileSync(resolve(vendor, 'MEMELOOP-SOURCE.patch'), 'utf8');
    assert.equal(digest(patch), '841a7ea7c4d7e8282f89ee15e4fa1d2cd4cd665c1f1b3018f086c09e1aebed42');
    const changed = [...patch.matchAll(/^\+\+\+ b\/(.+)$/gm)].map((match) => match[1]);
    assert.deepEqual(changed.sort(), ['Cargo.toml', 'src/ext.rs', 'src/ext/h2_attempt_observer.rs', 'src/proto/http2/client.rs']);
    const reversed = spawnSync('patch', ['-p1', '--reverse', '--batch', '--fuzz=0'], {
      cwd: patched, input: patch, encoding: 'utf8',
    });
    assert.equal(reversed.status, 0, reversed.stdout + reversed.stderr);
    assert.deepEqual(inventory(patched), upstream, 'unreviewed upstream source difference');
    const cargo = readFileSync(resolve(repository, 'Cargo.toml'), 'utf8');
    assert(cargo.includes('wreq-proto = { path = "vendor/wreq-proto" }'));
    const production = cargo.split('[dev-dependencies]')[0];
    assert(production !== undefined);
    assert(!production.includes('features = ["h2-attempt-observer"]'));
    const lock = readFileSync(resolve(repository, 'Cargo.lock'), 'utf8');
    const remainder = lock.split('name = "wreq-proto"')[1];
    assert(remainder !== undefined);
    const entry = remainder.split('[[package]]')[0];
    assert(entry !== undefined);
    assert(entry.includes('version = "0.2.6"'));
    assert(!entry.includes('source =') && !entry.includes('checksum ='));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
