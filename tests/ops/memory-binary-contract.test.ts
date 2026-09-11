import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { appendFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
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
