import assert from 'node:assert/strict';
import test from 'node:test';
import {
  findManagedRoute, inferManagedRouteProtocol, managedSyncTone, parseManagedModelSync,
} from '../src/operator/managedModelSync.js';
import type { ManagedModelSyncResponse, ModelRouteView } from '../src/types.js';

function response(overrides: Partial<ManagedModelSyncResponse['routes']> = {}): ManagedModelSyncResponse {
  return {
    catalog: {
      account_id: 'account', status: 'ready', credential_generation: 1,
      last_attempt_at: 1_800_000_000_000, last_success_at: 1_800_000_000_000, expires_at: 1_800_086_400_000,
      error_code: null, models: [{ id: 'model-a', protocol: 'openai', context_window: null, reservation_token_bound: null, reservation_bound_source: null }], disabled_models: [],
    },
    routes: { added: 1, disabled: 0, restored: 0, unchanged: 0, skipped: 0, warnings: [], ...overrides },
    price_sync: { status: 'deferred', currency: 'USD', imported: 0, preserved: 0, unmatched: 0, ambiguous: 0, failed_sources: [], error_code: 'managed_route_price_sync_deferred' },
  };
}

test('managed sync parser accepts the strict sync-routes contract', () => {
  const parsed = parseManagedModelSync(response());
  assert.equal(parsed.catalog.models.length, 1);
  assert.equal(parsed.routes.added, 1);
  assert.equal(parsed.price_sync.status, 'deferred');
});

test('managed sync parser rejects partial or shaped-wrong payloads', () => {
  for (const value of [
    undefined, null, {}, { catalog: response().catalog },
    { ...response(), price_sync: { status: 'ready' } },
    { ...response(), routes: { added: -1, disabled: 0, restored: 0, unchanged: 0, skipped: 0, warnings: [] } },
    { ...response(), routes: { added: 0.5, disabled: 0, restored: 0, unchanged: 0, skipped: 0, warnings: [] } },
    { ...response(), routes: { added: 0, disabled: 0, restored: 0, unchanged: 0, skipped: 0, warnings: 'sync_in_progress' } },
    { ...response(), catalog: { ...response().catalog, models: [{ id: 'model-a' }] } },
    { ...response(), catalog: { ...response().catalog, status: 'failed' } },
    { ...response(), catalog: { ...response().catalog, credential_generation: 1.5 } },
    { ...response(), catalog: { ...response().catalog, models: [{ id: 'model-a', protocol: 'openai', context_window: 0, reservation_token_bound: null, reservation_bound_source: null }] } },
    { ...response(), catalog: { ...response().catalog, models: [{ id: 'model-a', protocol: 'openai', context_window: null, reservation_token_bound: null }] } },
    { ...response(), price_sync: { ...response().price_sync, imported: 1 } },
    { ...response(), price_sync: { ...response().price_sync, failed_sources: ['litellm'] } },
  ]) assert.throws(() => parseManagedModelSync(value), /invalid managed sync response/);
});

test('warnings mark the outcome partial even when counters look clean', () => {
  assert.equal(managedSyncTone(response()), 'success');
  assert.equal(managedSyncTone(response({ warnings: ['catalog_not_ready'] })), 'partial');
  assert.equal(managedSyncTone(response({ skipped: 1 })), 'success', 'protected entries without warnings stay a clean success');
});

test('catalog protocols preserve supported values and only map the explicit wildcard', () => {
  assert.equal(inferManagedRouteProtocol('openai'), 'openai');
  assert.equal(inferManagedRouteProtocol('anthropic'), 'anthropic');
  assert.equal(inferManagedRouteProtocol('any'), 'openai');
  assert.equal(inferManagedRouteProtocol('vendor-specific'), undefined);
});

test('managed route lookup matches tenant account, model, and protocol together', () => {
  const route = {
    id: 'route-1', tenant_external_id: 'tenant', public_model: 'model-a', upstream_account_ids: ['account'],
    upstream_model: 'model-a', protocol: 'openai', priority: 0, enabled: true,
    created_at: 0, updated_at: 0, grant_revision: 0,
  } as ModelRouteView;
  assert.equal(findManagedRoute([route], 'account', 'model-a', 'openai')?.id, 'route-1');
  assert.equal(findManagedRoute([route], 'account', 'model-a', 'anthropic'), undefined);
  assert.equal(findManagedRoute([route], 'other-account', 'model-a', 'openai'), undefined);
  assert.equal(findManagedRoute([route], 'account', 'model-b', 'openai'), undefined);
  const legacy = { ...route, upstream_account_ids: undefined, upstream_account_id: 'account' } as ModelRouteView;
  assert.equal(findManagedRoute([legacy], 'account', 'model-a', 'openai')?.id, 'route-1', 'legacy single-account routes still match');
  const groupOnly = { ...route, upstream_account_ids: [], upstream_account_id: undefined, candidate_upstream_account_ids: ['account'] } as ModelRouteView;
  assert.equal(findManagedRoute([groupOnly], 'account', 'model-a', 'openai')?.id, 'route-1', 'effective provider-group candidates also cover the account');
});
