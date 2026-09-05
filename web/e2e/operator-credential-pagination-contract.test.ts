import assert from 'node:assert/strict';
import test from 'node:test';

import { translationCatalogs } from '../src/i18n.js';
import {
  applyKeyPage, canLoadMoreKeys, canReadCredentialLimits, canWriteCredential, credentialListPresentation,
  credentialStatuses, isOlderKeyCursor, keyListPageSize, keyListPath, matchesCredentialSearch,
  matchesCredentialStatus, ownsKeyListRequest, shouldLoadCredentialRoutes,
} from '../src/operator/keyPagination.js';
import type { KeyListCursor, KeyView } from '../src/types.js';

function key(index: number, createdAt = 10_000): KeyView {
  return {
    key_id: `00000000-0000-0000-0000-${String(index).padStart(12, '0')}`,
    alias: `Client ${index}`,
    principal_external_id: index % 2 === 0 ? `person-${index}` : undefined,
    currency: 'USD',
    credential_generation: 1,
    created_at: createdAt,
    policy: {
      requests_per_minute: 10,
      tokens_per_minute: 100,
      max_concurrency: 1,
      enforcement_mode: 'prepaid',
      daily_budget: null,
      weekly_budget: null,
      lifetime_budget: null,
    },
    available_balance: '0',
  };
}

function cursor(value: KeyView): KeyListCursor {
  return { before_created_at: value.created_at, before_id: value.key_id };
}

function apiPage(rows: KeyView[], before?: KeyListCursor) {
  return rows.filter((value) => !before || isOlderKeyCursor(cursor(value), before)).slice(0, keyListPageSize + 1);
}

test('managed-key pages carry the exact public exclusive keyset cursor', () => {
  const initial = new URL(keyListPath('tenant-a'), 'https://operator.example.test');
  assert.equal(initial.searchParams.get('tenant_external_id'), 'tenant-a');
  assert.equal(initial.searchParams.get('limit'), String(keyListPageSize + 1));
  assert.equal(initial.searchParams.get('before_created_at'), null);
  assert.equal(
    new URL(keyListPath(''), 'https://operator.example.test').searchParams.has('tenant_external_id'),
    false,
    'the all-tenants read-only view remains visible',
  );
});

test('same-created-at rows cross page boundaries without a duplicate or omission', () => {
  const source = Array.from({ length: keyListPageSize * 2 + 31 }, (_, index) => key(index + 1, 42))
    .sort((left, right) => right.key_id.localeCompare(left.key_id));
  let values: KeyView[] = [];
  let nextCursor: KeyListCursor | undefined;
  for (let pages = 0; pages < 10; pages += 1) {
    const applied = applyKeyPage(values, apiPage(source, nextCursor), nextCursor);
    assert.equal(applied.ok, true);
    if (!applied.ok) return;
    values = applied.values;
    nextCursor = applied.nextCursor;
    if (!nextCursor) break;
  }
  assert.equal(nextCursor, undefined, 'the final short page ends pagination');
  assert.deepEqual(values.map((value) => value.key_id), source.map((value) => value.key_id));
  assert.equal(new Set(values.map((value) => value.key_id)).size, source.length);
});

test('a repeated page or a cursor that does not advance terminates safely', () => {
  const source = Array.from({ length: keyListPageSize + 4 }, (_, index) => key(index + 1, 42))
    .sort((left, right) => right.key_id.localeCompare(left.key_id));
  const first = applyKeyPage([], apiPage(source));
  assert.equal(first.ok, true);
  if (!first.ok || !first.nextCursor) return;
  const repeated = applyKeyPage(first.values, apiPage(source), first.nextCursor);
  assert.equal(repeated.ok, false, 'a server replay cannot leave an endless load-more cursor');
  const duplicate = applyKeyPage(first.values, [...first.values, first.values[0]!], first.nextCursor);
  assert.equal(duplicate.ok, false, 'a duplicate row is rejected before it can corrupt the list');
});

test('search, all-tenant read permissions, and request ownership have explicit behavior', () => {
  assert.equal(matchesCredentialSearch(key(2), 'client 2', 'en'), true);
  assert.equal(matchesCredentialSearch(key(2), 'PERSON-2', 'en'), true);
  assert.equal(matchesCredentialSearch(key(1), 'person', 'en'), false);
  assert.deepEqual(credentialStatuses, ['active', 'suspended', 'revoked']);
  assert.equal(matchesCredentialStatus({ ...key(1), status: 'active' }, 'active'), true);
  assert.equal(matchesCredentialStatus({ ...key(1), status: 'suspended' }, 'active'), false);
  assert.equal(matchesCredentialStatus({ ...key(1), status: 'revoked' }, 'revoked'), true);
  assert.equal(canReadCredentialLimits(' mts_global '), true);
  assert.equal(canReadCredentialLimits(''), false);
  assert.equal(canWriteCredential('tenant-a'), true);
  assert.equal(canWriteCredential('  '), false, 'all-tenants remains read-only');

  const first = { generation: 1, scopeGeneration: 4 };
  const replacement = { generation: 2, scopeGeneration: 4 };
  assert.equal(ownsKeyListRequest(replacement, first), false, 'an aborted request cannot release its replacement');
  assert.equal(ownsKeyListRequest(replacement, replacement), true);
  assert.equal(canLoadMoreKeys('more', true, false), true);
  assert.equal(
    canLoadMoreKeys('more', true, true),
    false,
    'a second click is ignored while the first request owns the cursor',
  );
  assert.equal(canLoadMoreKeys('failed', true, false), false, 'a failed page cannot keep retrying the same cursor');
  assert.equal(shouldLoadCredentialRoutes(''), false, 'all-tenants key review does not depend on routes');
  assert.equal(shouldLoadCredentialRoutes('tenant-a'), true);
  assert.equal(credentialListPresentation('complete', false), 'complete');
  assert.equal(credentialListPresentation('failed', false), 'failed', 'a failed key load cannot claim completion even if another resource failed separately');
});

test('credential list copy remains bilingual and distinguishes loading, failure, and completion', () => {
  const zh = translationCatalogs['zh-CN'];
  const en = translationCatalogs.en;
  for (const key of [
    'credentials.loadingList', 'credentials.loadingMore', 'credentials.loadFailed',
    'credentials.loadedComplete', 'credentials.retryLoad', 'credentials.paginationStalled', 'credentials.tenant',
  ] as const) {
    assert.notEqual(zh[key], undefined);
    assert.notEqual(en[key], undefined);
  }
  assert.notEqual(zh['credentials.loadFailed'], zh['credentials.loadedComplete']);
  assert.notEqual(en['credentials.loadFailed'], en['credentials.loadedComplete']);
});
