import assert from 'node:assert/strict';
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { parse, stringify } from 'yaml';
import { rejected, repository, run } from './contract-helpers.ts';

type Workflow = Record<string, any>;

test('GitHub workflow policy rejects malicious fixtures', () => {
  const fixture = mkdtempSync(join(tmpdir(), 'mtc-github-policy-fixture-'));
  try {
    const workflow = join(fixture, '.github/workflows/ci.yml');
    mkdirSync(join(fixture, '.github/workflows'), { recursive: true });
    run('git', ['-C', fixture, 'init', '--quiet']);
    const writeGood = (): void => {
      cpSync(join(repository, '.github/workflows/ci.yml'), workflow);
      writeFileSync(join(fixture, 'README.md'), 'clean fixture\n');
      run('git', ['-C', fixture, 'add', '--all']);
    };
    const verify = (): void => {
      run(process.execPath, [join(repository, 'web/scripts/verify-github-workflow-policy.mjs'), workflow, fixture]);
    };
    const expectRejected = (label: string): void => {
      rejected(process.execPath, [join(repository, 'web/scripts/verify-github-workflow-policy.mjs'), workflow, fixture]);
      assert.ok(label.length > 0);
    };
    const mutate = (mode: string): void => {
      const payload = parse(readFileSync(workflow, 'utf8')) as Workflow;
      const publish = payload.jobs['publish-ghcr'];
      const dependency = payload.jobs['dependency-security'];
      const buildStep = publish.steps.find((step: Workflow) => String(step.uses ?? '').startsWith('docker/build-push-action@'));
      switch (mode) {
        case 'top-permissions': payload.permissions.actions = 'write'; break;
        case 'publish-permissions': publish.permissions.packages = 'read'; break;
        case 'other-packages-write': payload.jobs.rust.permissions = { contents: 'read', packages: 'write' }; break;
        case 'other-write-all': payload.jobs.rust.permissions = 'write-all'; break;
        case 'other-actions-write': payload.jobs.rust.permissions = { actions: 'write' }; break;
        case 'other-extra-scope': payload.jobs.rust.permissions = { contents: 'read', checks: 'read' }; break;
        case 'rustsec-folded-ignore': {
          const audit = dependency.steps.find((step: Workflow) => String(step.run ?? '').includes('cargo audit'));
          audit.run = 'cargo audit --deny warnings\n  --ignore RUSTSEC-2099-0001'; break;
        }
        case 'exporter-downgrade': buildStep.with.outputs = 'type=image,push=true'; break;
        case 'push-shorthand-conflict': buildStep.with.push = true; break;
        default: throw new Error(`unknown mutation: ${mode}`);
      }
      writeFileSync(workflow, stringify(payload));
    };

    writeGood(); verify();
    for (const mode of [
      'top-permissions', 'publish-permissions', 'other-packages-write', 'other-write-all',
      'other-actions-write', 'other-extra-scope', 'rustsec-folded-ignore',
      'exporter-downgrade', 'push-shorthand-conflict',
    ]) { writeGood(); mutate(mode); expectRejected(mode); }

    writeGood();
    writeFileSync(join(fixture, 'retired-owner.txt'), 'https://github.com/linonetwo/memeloop-token-center\n');
    run('git', ['-C', fixture, 'add', 'retired-owner.txt']);
    expectRejected('retired-self-owner');
  } finally { rmSync(fixture, { recursive: true, force: true }); }
});
