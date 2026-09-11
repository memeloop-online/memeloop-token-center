import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ConvergenceFailure,
  buildPlan,
  converge,
  manifestSha256,
  parseManifest,
  type AccountCatalog,
  type ControlApi,
  type LiveAccount,
  type LiveRoute,
  type ReviewedManifest,
  type RoutePlan,
} from '../../ops/release/converge-codex-model-entitlements.ts';

const ids = Array.from({ length: 12 }, (_, index) => `00000000-0000-4000-8000-${String(index + 1).padStart(12, '0')}`);
const names = ['Fictional Alpha', 'Fictional Beta', 'Fictional Gamma', 'Fictional Delta', 'Fictional Epsilon'];
const models = ['gpt-5.6-luna', 'gpt-5.6-terra', 'gpt-6-astra'] as const;

function rawManifest(): Record<string, unknown> {
  const accounts = names.map((name, index) => ({ account_id: ids[index], name, driver: 'openai-codex', auth_kind: 'oauth', connection_method: 'oauth', credential_generation: 10 + index, updated_at: 100 + index, astra: index < 4 }));
  const routes = Object.fromEntries(models.map((model, index) => [model, { route_id: ids[5 + index], public_model: model, upstream_model: model, protocol: 'openai', priority: index, enabled: true, updated_at: 200 + index, grant_revision: 20 + index, upstream_account_ids: [ids[0]], included_provider_group_ids: [], excluded_provider_group_ids: [], route_group_ids: [ids[8]], granted_credential_ids: [ids[9]], custom_model_confirmed: true }]));
  return { schema_version: 1, tenant_external_id: 'tenant-a', target_base_url: 'https://control.example/', accounts, routes };
}

function fixture() {
  const manifest = parseManifest(rawManifest() as never);
  const accounts: LiveAccount[] = manifest.accounts.map(item => ({ id: item.account_id, tenant_external_id: manifest.tenant_external_id, name: item.name, driver: item.driver, auth_kind: item.auth_kind, connection_method: item.connection_method, credential_generation: item.credential_generation, status: 'active', updated_at: item.updated_at }));
  accounts.push({ id: ids[10]!, tenant_external_id: manifest.tenant_external_id, name: 'ordinary HTTP', driver: 'http-json', auth_kind: 'api_key', connection_method: 'api_key', credential_generation: 1, status: 'active', updated_at: 1 });
  const routes: LiveRoute[] = models.map(model => {
    const item = manifest.routes[model];
    return { id: item.route_id, tenant_external_id: manifest.tenant_external_id, public_model: item.public_model, upstream_model: item.upstream_model, protocol: item.protocol, priority: item.priority, enabled: item.enabled, updated_at: item.updated_at, grant_revision: item.grant_revision, upstream_account_ids: [...item.upstream_account_ids], included_provider_group_ids: [], excluded_provider_group_ids: [], route_group_ids: [...item.route_group_ids], granted_credential_ids: [...item.granted_credential_ids], custom_model_confirmed: item.custom_model_confirmed };
  });
  const catalogs = new Map<string, AccountCatalog>(manifest.accounts.map(account => [account.account_id, { account_id: account.account_id, status: 'ready', credential_generation: account.credential_generation, models: models.map(id => ({ id, protocol: 'openai' })) }]));
  return { manifest, accounts, routes, catalogs };
}

function code(action: () => unknown, expected: string): void {
  assert.throws(action, (error: unknown) => error instanceof ConvergenceFailure && error.code === expected);
}

test('manifest requires exactly four Astra accounts and native Codex identity pins', () => {
  const missing = rawManifest(); (missing.accounts as Array<{ astra: boolean }>)[3]!.astra = false;
  code(() => parseManifest(missing as never), 'astra_account_set_invalid');
  const wrongDriver = rawManifest(); (wrongDriver.accounts as Array<{ driver: string }>)[0]!.driver = 'http-json';
  code(() => parseManifest(wrongDriver as never), 'manifest_invalid');
});

test('canonical approval binds the private manifest account identities and Astra selection', () => {
  const reviewed = parseManifest(rawManifest() as never);
  const renamed = rawManifest(); (renamed.accounts as Array<{ name: string }>)[0]!.name = 'Fictional Renamed';
  const reassigned = rawManifest();
  (reassigned.accounts as Array<{ astra: boolean }>)[0]!.astra = false;
  (reassigned.accounts as Array<{ astra: boolean }>)[4]!.astra = true;
  assert.notEqual(manifestSha256(parseManifest(renamed as never)), manifestSha256(reviewed));
  assert.notEqual(manifestSha256(parseManifest(reassigned as never)), manifestSha256(reviewed));
});

test('Luna and Terra select every reviewed native account while Astra selects exactly four', () => {
  const { manifest, accounts, routes, catalogs } = fixture();
  const plan = buildPlan(manifest, accounts, routes, catalogs);
  assert.deepEqual(plan.find(item => item.model === 'gpt-5.6-luna')!.desired_account_ids, manifest.accounts.map(item => item.account_id));
  assert.deepEqual(plan.find(item => item.model === 'gpt-5.6-terra')!.desired_account_ids, manifest.accounts.map(item => item.account_id));
  assert.deepEqual(plan.find(item => item.model === 'gpt-6-astra')!.desired_account_ids, manifest.accounts.filter(item => item.astra).map(item => item.account_id));
  assert.ok(plan.every(item => item.outcome === 'change'));
});

test('missing, extra, renamed, or rotated native accounts fail closed', () => {
  const first = fixture(); first.accounts.splice(2, 1);
  code(() => buildPlan(first.manifest, first.accounts, first.routes, first.catalogs), 'native_account_set_changed');
  const second = fixture(); second.accounts.push({ ...second.accounts[0]!, id: ids[11]!, name: 'new native account' });
  code(() => buildPlan(second.manifest, second.accounts, second.routes, second.catalogs), 'native_account_set_changed');
  const third = fixture(); third.accounts[0] = { ...third.accounts[0]!, name: 'renamed' };
  code(() => buildPlan(third.manifest, third.accounts, third.routes, third.catalogs), 'account_cas_mismatch');
  const fourth = fixture(); fourth.accounts[0] = { ...fourth.accounts[0]!, credential_generation: 99 };
  code(() => buildPlan(fourth.manifest, fourth.accounts, fourth.routes, fourth.catalogs), 'account_cas_mismatch');
});

test('stale trusted catalogs remain routable but unknown or unsupported catalogs fail closed', () => {
  const stale = fixture(); stale.catalogs.get(ids[0]!)!.status = 'stale';
  assert.equal(buildPlan(stale.manifest, stale.accounts, stale.routes, stale.catalogs).length, 3);
  const unknown = fixture(); unknown.catalogs.get(ids[0]!)!.status = 'unknown';
  code(() => buildPlan(unknown.manifest, unknown.accounts, unknown.routes, unknown.catalogs), 'catalog_not_routable');
  const unsupported = fixture(); unsupported.catalogs.get(ids[0]!)!.models = unsupported.catalogs.get(ids[0]!)!.models.filter(item => item.id !== 'gpt-5.6-luna');
  code(() => buildPlan(unsupported.manifest, unsupported.accounts, unsupported.routes, unsupported.catalogs), 'catalog_not_routable');
});

class FakeControl implements ControlApi {
  updates: RoutePlan[] = [];
  private readonly state: ReturnType<typeof fixture>;

  constructor(state: ReturnType<typeof fixture>) { this.state = state; }
  async accounts(): Promise<LiveAccount[]> { return this.state.accounts; }
  async routes(): Promise<LiveRoute[]> { return this.state.routes; }
  async catalog(_tenant: string, accountId: string): Promise<AccountCatalog> { return this.state.catalogs.get(accountId)!; }
  async update(_tenant: string, item: RoutePlan): Promise<LiveRoute> {
    this.updates.push(item); return { ...item.route, updated_at: item.route.updated_at + 1, upstream_account_ids: item.desired_account_ids, included_provider_group_ids: [], excluded_provider_group_ids: [], custom_model_confirmed: false };
  }
}

test('dry-run performs no writes and apply preserves grants while converging three routes', async () => {
  const dryFixture = fixture(); const dry = new FakeControl(dryFixture);
  const summary = await converge(dryFixture.manifest, dry, false);
  assert.equal(summary.mode, 'dry-run'); assert.equal(summary.planned_change_count, 3); assert.equal(dry.updates.length, 0);
  const applyFixture = fixture(); const apply = new FakeControl(applyFixture);
  const applied = await converge(applyFixture.manifest, apply, true);
  assert.equal(applied.changed_count, 3); assert.equal(apply.updates.length, 3);
  assert.ok(apply.updates.every(item => item.route.granted_credential_ids[0] === ids[9] && item.route.route_group_ids[0] === ids[8]));
});

test('an exact desired topology is an idempotent replay even after route timestamps advance', () => {
  const value = fixture(); const all = value.manifest.accounts.map(item => item.account_id); const astra = value.manifest.accounts.filter(item => item.astra).map(item => item.account_id);
  for (const route of value.routes) { route.upstream_account_ids = route.public_model === 'gpt-6-astra' ? astra : all; route.custom_model_confirmed = false; route.updated_at += 500; }
  const plan = buildPlan(value.manifest, value.accounts, value.routes, value.catalogs);
  assert.ok(plan.every(item => item.outcome === 'replay'));
});

test('route grants, provider groups, and target identity are pinned by the reviewed manifest', () => {
  const grant = fixture(); grant.routes[0]!.granted_credential_ids = [ids[10]!];
  code(() => buildPlan(grant.manifest, grant.accounts, grant.routes, grant.catalogs), 'route_cas_mismatch');
  const identity = fixture(); identity.routes[0]!.upstream_model = 'other';
  code(() => buildPlan(identity.manifest, identity.accounts, identity.routes, identity.catalogs), 'route_cas_mismatch');
});

test('an unexpected enabled route or duplicated live relation fails closed', () => {
  const extra = fixture();
  extra.routes.push({ ...extra.routes[2]!, id: ids[11]!, priority: 99 });
  code(() => buildPlan(extra.manifest, extra.accounts, extra.routes, extra.catalogs), 'route_set_changed');
  const duplicate = fixture(); duplicate.routes[0]!.upstream_account_ids.push(ids[0]!);
  code(() => buildPlan(duplicate.manifest, duplicate.accounts, duplicate.routes, duplicate.catalogs), 'route_cas_mismatch');
});
