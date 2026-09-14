import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { appendFileSync, chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { rejected, repository, run } from './contract-helpers.ts';

test('memory acceptance binary artifacts remain bound to their exact revision and digest', () => {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-memory-binary-contract-'));
  const revision = '1'.repeat(40);
  try {
    const artifact = join(temporary, 'artifact');
    const destinationDirectory = join(temporary, 'destination');
    const destination = join(destinationDirectory, 'memeloop-token-center');
    run(process.execPath, [
      'ops/ci/create-memory-binary-manifest.ts',
      process.execPath,
      revision,
      artifact,
    ]);
    mkdirSync(destinationDirectory);
    run(process.execPath, [
      'ops/ci/install-memory-binary.ts',
      artifact,
      revision,
      destination,
    ]);
    assert.deepEqual(readFileSync(destination), readFileSync(process.execPath));
    assert.equal(statSync(destination).mode & 0o777, 0o500);

    const tamperedDestinationDirectory = join(temporary, 'tampered-destination');
    mkdirSync(tamperedDestinationDirectory);
    appendFileSync(join(artifact, 'memeloop-token-center'), 'tampered');
    rejected(process.execPath, [
      'ops/ci/install-memory-binary.ts',
      artifact,
      revision,
      join(tamperedDestinationDirectory, 'memeloop-token-center'),
    ], { cwd: repository });
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
});

test('memory binary installer reports all missing required positional arguments accurately', () => {
  const result = spawnSync(process.execPath, [
    'ops/ci/install-memory-binary.ts',
    '/tmp/memory-binary-artifact',
    '',
    '/tmp/memeloop-token-center',
  ], { cwd: repository, encoding: 'utf8', shell: false });

  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /memory binary installation: artifact directory, revision, and destination are required/);
});

test('Docker-native service release inputs bind binary, runtime libraries, feature set, platform, and revision', () => {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-release-service-input-contract-'));
  const revision = '2'.repeat(40);
  try {
    const exported = join(temporary, 'exported');
    const artifact = join(temporary, 'artifact');
    mkdirSync(exported);
    copyFileSync(process.execPath, join(exported, 'memeloop-token-center'));
    writeFileSync(join(exported, 'libgcc_s.so.1'), 'gcc runtime');
    writeFileSync(join(exported, 'libstdc++.so.6'), 'cxx runtime');
    run(process.execPath, ['ops/ci/create-memory-binary-manifest.ts', join(exported, 'memeloop-token-center'), revision, artifact]);
    copyFileSync(join(exported, 'libgcc_s.so.1'), join(artifact, 'libgcc_s.so.1'));
    copyFileSync(join(exported, 'libstdc++.so.6'), join(artifact, 'libstdc++.so.6'));
    run(process.execPath, ['ops/ci/create-release-service-input-manifest.ts', artifact, revision]);
    // GitHub artifact download normalizes file modes. The final Dockerfile
    // restores the executable bit while digest verification remains valid.
    chmodSync(join(artifact, 'memeloop-token-center'), 0o644);
    run(process.execPath, ['ops/ci/verify-release-service-input.ts', artifact, revision]);

    appendFileSync(join(artifact, 'libstdc++.so.6'), 'tampered');
    rejected(process.execPath, ['ops/ci/verify-release-service-input.ts', artifact, revision], { cwd: repository });
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
});
