import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { rejected, repository, run } from './contract-helpers.ts';

type Change = readonly [status: string, ...paths: string[]];

function encoded(changes: readonly Change[]): string {
  return changes.flatMap(([status, ...paths]) => [status, ...paths]).join('\0') + (changes.length === 0 ? '' : '\0');
}

function scopes(event: 'pull_request' | 'push', changes: readonly Change[]): Record<string, string> {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-ci-scope-contract-'));
  try {
    const changed = join(temporary, 'changed.z');
    const output = join(temporary, 'output.txt');
    writeFileSync(changed, encoded(changes));
    writeFileSync(output, '');
    run(process.execPath, ['ops/ci/detect-expensive-ci-scopes.ts', event, changed, output]);
    return Object.fromEntries(readFileSync(output, 'utf8').trim().split('\n').map((line) => line.split('=', 2)));
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

const webOnly = { rust: 'false', web: 'true', migration: 'false', memory: 'false', memory_acceptance: 'false', plugin_installer: 'false' };
const staticContractsOnly = { rust: 'false', web: 'false', migration: 'false', memory: 'false', memory_acceptance: 'false', plugin_installer: 'false' };
const full = { rust: 'true', web: 'true', migration: 'true', memory: 'true', memory_acceptance: 'true', plugin_installer: 'false' };
const fullPlugin = { ...full, plugin_installer: 'true' };

test('verified merge scope ignores a stale event base and fails closed for an unverified checkout', () => {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-ci-merge-scope-contract-'));
  const env = {
    ...process.env,
    GIT_CONFIG_GLOBAL: '/dev/null',
    GIT_CONFIG_NOSYSTEM: '1',
    GIT_AUTHOR_NAME: 'CI fixture', GIT_AUTHOR_EMAIL: 'fixture@example.test',
    GIT_COMMITTER_NAME: 'CI fixture', GIT_COMMITTER_EMAIL: 'fixture@example.test',
    GIT_AUTHOR_DATE: '2026-01-01T00:00:00Z', GIT_COMMITTER_DATE: '2026-01-01T00:00:00Z',
  };
  const git = (...args: string[]): string => run('git', args, { cwd: temporary, env }).trim();
  const commit = (message: string, ...parents: string[]): string => git(
    'commit-tree', git('write-tree'), ...parents.flatMap((parent) => ['-p', parent]), '-m', message,
  );
  try {
    git('init', '--quiet', '--initial-branch=fixture');
    mkdirSync(join(temporary, 'src'));
    mkdirSync(join(temporary, 'web'));
    writeFileSync(join(temporary, 'src/runtime.rs'), 'old base\n');
    writeFileSync(join(temporary, 'web/App.tsx'), 'old UI\n');
    git('add', '.');
    const eventBase = commit('original event base');
    writeFileSync(join(temporary, 'web/App.tsx'), 'new UI\n');
    git('add', 'web/App.tsx');
    const prHead = commit('web-only PR', eventBase);
    git('read-tree', eventBase);
    writeFileSync(join(temporary, 'src/runtime.rs'), 'unrelated base advance\n');
    git('add', 'src/runtime.rs');
    const actualBase = commit('base advanced after event snapshot', eventBase);
    git('read-tree', prHead);
    git('add', 'src/runtime.rs');
    const merge = commit('actual checked out merge', actualBase, prHead);
    git('update-ref', 'HEAD', merge);
    assert.match(git('diff', '--name-only', eventBase, merge), /^src\/runtime\.rs$/m,
      'the stale event-base comparison must reproduce the unrelated runtime change');

    const resolve = (checkout: string, head: string): { scopes: Record<string, string>; evidence: Record<string, unknown> } => {
      const output = join(temporary, 'scope-output.txt');
      writeFileSync(output, '');
      const result = run(process.execPath, [join(repository, 'ops/ci/detect-expensive-ci-scopes.ts'), 'pull_request', '--verified-merge', output], {
        cwd: temporary, env: { ...env, BASE_SHA: eventBase, GITHUB_SHA: checkout, PR_HEAD_SHA: head },
      });
      return {
        scopes: Object.fromEntries(readFileSync(output, 'utf8').trim().split('\n').map((line) => line.split('=', 2))),
        evidence: JSON.parse(result),
      };
    };
    const verified = resolve(merge, prHead);
    assert.deepEqual(verified.scopes, webOnly);
    assert.equal(verified.evidence.comparison_base, actualBase);
    assert.equal(verified.evidence.change_count, 1);
    assert.equal(verified.evidence.force_full, false);
    for (const [checkout, head] of [[merge, actualBase], [merge, 'not-a-sha'], [prHead, prHead]]) {
      const fallback = resolve(checkout!, head!);
      assert.deepEqual(fallback.scopes, fullPlugin);
      assert.equal(fallback.evidence.force_full, true);
    }
    git('update-ref', 'HEAD', prHead);
    assert.deepEqual(resolve(prHead, prHead).scopes, fullPlugin, 'one-parent checkout must not grant skips');
    const octopus = commit('unexpected three-parent merge', actualBase, prHead, eventBase);
    git('update-ref', 'HEAD', octopus);
    assert.deepEqual(resolve(octopus, prHead).scopes, fullPlugin, 'three-parent checkout must not grant skips');
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
});

test('scope matrix skips expensive service gates only for presentation or static-contract pull requests', () => {
  const matrix: readonly [label: string, changes: readonly Change[], expected: Record<string, string>][] = [
    ['PR 66-shaped web source and browser contract', [['M', 'web/src/useAnchoredPopover.ts'], ['A', 'web/e2e/popover-width-browser-contract.test.ts']], webOnly],
    ['web deletion', [['D', 'web/e2e/obsolete-browser-contract.test.ts']], webOnly],
    ['web-only rename', [['R100', 'web/src/old.tsx', 'web/src/new.tsx']], webOnly],
    ['PR 251-shaped browser source, fixture, and design documentation', [
      ['M', 'docs/fluent-design-system.md'],
      ['M', 'web/e2e/fixtures/operator-overview.tsx'],
      ['M', 'web/src/operator/pages/RequestsPage.tsx'],
    ], webOnly],
    ['PR 115-shaped source module static contract', [['M', 'tests/ops/source-module-size-contract.test.ts']], staticContractsOnly],
    ['static contract helper', [['M', 'tests/ops/contract-helpers.ts']], staticContractsOnly],
    ['static contract plus runtime source', [['M', 'tests/ops/source-module-size-contract.test.ts'], ['M', 'src/api/routes/control.rs']], fullPlugin],
    ['web build script', [['M', 'web/scripts/verify-github-workflow-policy.mjs']], webOnly],
    ['CI scope script', [['M', 'ops/ci/detect-expensive-ci-scopes.ts']], full],
    ['production renamed into web', [['R100', 'src/api/routes.rs', 'web/src/routes.ts']], fullPlugin],
    ['web renamed into production', [['R100', 'web/src/routes.ts', 'src/api/routes.rs']], fullPlugin],
    ['documentation renamed into production', [['R100', 'docs/operator.md', 'src/operator.rs']], fullPlugin],
    ['shared manifest', [['M', 'package-lock.json']], full],
    ['Rust manifest', [['M', 'Cargo.lock']], fullPlugin],
    ['migration', [['D', 'migrations/0099_retired.sql']], fullPlugin],
    ['CI workflow', [['M', '.github/workflows/ci.yml']], fullPlugin],
    ['unknown path', [['A', 'future-runtime-input.txt']], full],
  ];
  for (const [label, changes, expected] of matrix) assert.deepEqual(scopes('pull_request', changes), expected, label);
});

test('known documentation and ordinary test paths preserve existing memory policy but not Rust or migration coverage', () => {
  assert.deepEqual(
    scopes('pull_request', [['M', 'docs/performance.md'], ['M', 'tests/route_management.rs']]),
    { rust: 'true', web: 'true', migration: 'true', memory: 'false', memory_acceptance: 'false', plugin_installer: 'false' },
  );
});

test('the focused plugin UI workflow is manual because mandatory web CI already covers its browser contracts', () => {
  const pluginWorkflow = readFileSync(join(repository, '.github/workflows/plugin-ui-projection.yml'), 'utf8');
  const ciWorkflow = readFileSync(join(repository, '.github/workflows/ci.yml'), 'utf8');
  const webPackage = JSON.parse(readFileSync(join(repository, 'web/package.json'), 'utf8')) as { scripts?: Record<string, string> };

  assert.match(pluginWorkflow, /^  workflow_dispatch:/m);
  assert.doesNotMatch(pluginWorkflow, /^  pull_request:/m);
  assert.match(ciWorkflow, /MTC_REQUIRE_BROWSER=1 npm run test:localization/);
  const mandatoryContracts = webPackage.scripts?.['test:localization'] ?? '';
  assert.match(mandatoryContracts, /e2e\/\*-contract\.test\.ts/);
  for (const contract of [
    'e2e/plugin-ui-projection-contract.test.ts',
    'e2e/plugin-ui-slot-browser-contract.test.ts',
    'e2e/operator-plugin-projection-browser-contract.test.ts',
  ]) assert.match(contract, /^e2e\/[^/]+-contract\.test\.ts$/);
});

test('pushes and malformed or empty pull-request diffs fail closed', () => {
  assert.deepEqual(scopes('push', [['M', 'web/src/App.tsx']]), fullPlugin);
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-ci-scope-contract-'));
  try {
    const changed = join(temporary, 'changed.z');
    const output = join(temporary, 'output.txt');
    writeFileSync(output, '');
    writeFileSync(changed, '');
    rejected(process.execPath, ['ops/ci/detect-expensive-ci-scopes.ts', 'pull_request', changed, output]);
    writeFileSync(changed, 'R100\0web/src/old.tsx\0');
    rejected(process.execPath, ['ops/ci/detect-expensive-ci-scopes.ts', 'pull_request', changed, output]);
    writeFileSync(changed, 'X\0web/src/App.tsx\0');
    rejected(process.execPath, ['ops/ci/detect-expensive-ci-scopes.ts', 'pull_request', changed, output]);
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
});

test('verified push scopes use the complete before-after range and fail closed without it', () => {
  const temporary = mkdtempSync(join(tmpdir(), 'mtc-ci-push-scope-'));
  const env = { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1', GIT_AUTHOR_NAME: 'CI fixture', GIT_AUTHOR_EMAIL: 'fixture@example.test', GIT_COMMITTER_NAME: 'CI fixture', GIT_COMMITTER_EMAIL: 'fixture@example.test' };
  const git = (...args: string[]): string => run('git', args, { cwd: temporary, env }).trim();
  try {
    git('init', '--quiet', '--initial-branch=fixture'); mkdirSync(join(temporary, 'web'));
    writeFileSync(join(temporary, 'web', 'a.tsx'), 'one\n'); git('add', '.'); const before = git('commit-tree', git('write-tree'), '-m', 'before');
    writeFileSync(join(temporary, 'web', 'a.tsx'), 'two\n'); git('add', '.'); const middle = git('commit-tree', git('write-tree'), '-p', before, '-m', 'web');
    writeFileSync(join(temporary, 'web', 'b.tsx'), 'three\n'); git('add', '.'); const after = git('commit-tree', git('write-tree'), '-p', middle, '-p', before, '-m', 'web merge'); git('update-ref', 'HEAD', after);
    const output = join(temporary, 'out'); writeFileSync(output, '');
    run(process.execPath, [join(repository, 'ops/ci/detect-expensive-ci-scopes.ts'), 'push', '--verified-merge', output], { cwd: temporary, env: { ...env, GITHUB_SHA: after, GITHUB_EVENT_BEFORE: before, GITHUB_EVENT_AFTER: after } });
    const result = Object.fromEntries(readFileSync(output, 'utf8').trim().split('\n').map(line => line.split('=', 2)));
    assert.equal(result.memory, 'true'); assert.equal(result.memory_acceptance, 'false'); assert.equal(result.rust, 'false');
    writeFileSync(output, ''); run(process.execPath, [join(repository, 'ops/ci/detect-expensive-ci-scopes.ts'), 'push', '--verified-merge', output], { cwd: temporary, env: { ...env, GITHUB_SHA: after, GITHUB_EVENT_BEFORE: '', GITHUB_EVENT_AFTER: after } });
    assert.equal(Object.fromEntries(readFileSync(output, 'utf8').trim().split('\n').map(line => line.split('=', 2))).memory_acceptance, 'true');
  } finally { rmSync(temporary, { recursive: true, force: true }); }
});

test('publisher admits only successful required jobs and scope-authorized skips', () => {
  const workflow = readFileSync(join(repository, '.github/workflows/ci.yml'), 'utf8');
  const publish = workflow.slice(workflow.indexOf('  publish-ghcr:'), workflow.indexOf('\n  verify-ghcr-release:'));
  assert.match(publish, /needs:\n(?:.*\n)*?      - changes\n/);
  assert.match(publish, /needs:\n(?:.*\n)*?      - memory-binary\n/);
  for (const condition of [
    "needs.changes.result == 'success'", "needs.memory-binary.result == 'success'",
    "needs.web.result == 'success' || (needs.changes.outputs.web == 'false' && needs.web.result == 'skipped')",
    "needs.rust.result == 'success' || (needs.changes.outputs.rust == 'false' && needs.rust.result == 'skipped')",
    "needs.migration-smoke.result == 'success' || (needs.changes.outputs.migration == 'false' && needs.migration-smoke.result == 'skipped')",
    "needs.memory-acceptance.result == 'success' || (needs.changes.outputs.memory_acceptance == 'false' && needs.memory-acceptance.result == 'skipped')",
  ]) assert.match(publish, new RegExp(condition.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
  assert.doesNotMatch(publish, /\.result != 'failure'/);
});
