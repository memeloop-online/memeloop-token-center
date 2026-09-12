import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';
import { repository } from './contract-helpers.ts';

const readme = readFileSync(resolve(repository, 'README.md'), 'utf8');

function trackedTextFiles(): Array<{ path: string; text: string }> {
  const paths = execFileSync('git', ['ls-files', '-z'], { cwd: repository })
    .toString('utf8')
    .split('\0')
    .filter(Boolean);
  return paths.flatMap((path) => {
    const contents = readFileSync(resolve(repository, path));
    return contents.includes(0) ? [] : [{ path, text: contents.toString('utf8') }];
  });
}

test('README omits deployed addresses and links only to generic project documentation', () => {
  assert.doesNotMatch(readme, /https?:\/\//u);
  assert.match(readme, /\[project overview\]\(docs\/project-overview\.md\)/u);
  assert.match(readme, /approved operational procedures/u);
});

test('README contains no credential-retrieval commands', () => {
  const blocks = [...readme.matchAll(/```([^\n]+)\n([\s\S]*?)\n```/gu)];
  assert.deepEqual(blocks, []);
  assert.doesNotMatch(readme, /kubectl|\bget secret\b|base64|service-token|jsonpath/u);
});

test('tracked sources omit private deployment markers, trial topology, and credential retrieval commands', () => {
  const forbiddenLiterals = [
    ['private deployment domain', 'one' + 'two.website'],
    ['retired trial environment', 'api2' + '-trial'],
    ['retired deployment identity', 'memeloop-token-center-' + 'api2'],
    ['trial-only topology guidance', 'trial-' + 'only static name mappings'],
    ['trial provider topology', 'trial ' + 'provider'],
  ] as const;
  const credentialRead = new RegExp(
    String.raw`kubectl[^\n]{0,240}\bget\b[^\n]{0,80}\b(?:secret|secrets)\b`,
    'iu',
  );

  for (const { path, text } of trackedTextFiles()) {
    for (const [label, literal] of forbiddenLiterals) {
      assert.equal(text.includes(literal), false, `${path} exposes ${label}`);
    }
    assert.doesNotMatch(text, credentialRead, `${path} contains a credential retrieval command`);
  }
});

test('retired in-repository migration delivery surfaces stay absent', () => {
  const paths = execFileSync('git', ['ls-files', '-z'], { cwd: repository })
    .toString('utf8')
    .split('\0')
    .filter(Boolean);
  const retiredPaths = [
    'src/bin/' + 'import-cpa-' + 'session-archive.rs',
    'src/api/upstreams/' + 'managed_import.rs',
    'src/api/upstreams/' + 'native_codex_upgrade.rs',
    'src/api/' + 'archive_quarantine.rs',
    'src/db/providers/' + 'imports.rs',
    'src/db/requests/' + 'session_archive_quarantine.rs',
    'src/' + 'session_archive_import',
    'docs/evidence',
    'docs/integrations/' + 'cloud-pr4-review-20260909.md',
    'docs/integrations/' + 'cloud-principal-ensure-review.md',
    'docs/experience-' + 'acceptance-20260909.md',
  ];
  for (const retired of retiredPaths) {
    assert.equal(
      paths.some((path) => path === retired || path.startsWith(`${retired}/`)),
      false,
      `${retired} is a retired migration, import, or deployment-evidence surface`,
    );
  }

  const retiredApis = [
    '/internal/v1/imports/' + 'cpa/managed-oauth',
    '/internal/v1/migrations/' + 'openai-codex',
    '/internal/v1/imports/' + 'session-archive/quarantine',
  ];
  const retiredSymbols = [
    'import-cpa-' + 'session-archive',
    'CARGO_BIN_EXE_' + 'import-cpa-' + 'session-archive',
    'pub mod ' + 'session_archive_import',
    'session_archive_' + 'commit',
    'import_' + 'cpa_managed_oauth_account',
    'SessionArchive' + 'CommitInput',
    'commit_session_archive_' + 'quarantine',
    'normalize_managed_' + 'oauth_document',
    'ManagedOAuthAdapter' + 'Contribution',
    'ManagedOAuthNormalized' + 'Account',
    '"managed_' + 'oauth_adapter"',
    'cpa-managed-' + 'oauth-adapter-v1',
    'NativeCodex' + 'Upgrade',
    'native_codex_' + 'upgrade',
    'SessionArchive' + 'Quarantine',
    'imports:session_archive:' + 'quarantine:',
  ];
  for (const { path, text } of trackedTextFiles()) {
    for (const retiredApi of retiredApis) {
      assert.equal(text.includes(retiredApi), false, `${path} exposes retired API ${retiredApi}`);
    }
    for (const symbol of retiredSymbols) {
      assert.equal(text.includes(symbol), false, `${path} references retired symbol ${symbol}`);
    }
  }
});
