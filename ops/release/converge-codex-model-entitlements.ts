/** Fail-closed Codex route convergence. Dry-run is the default and performs GETs only. */
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/u;
const ASTRA_ACCOUNT_COUNT = 4;
const MODEL_IDS = ['gpt-5.6-luna', 'gpt-5.6-terra', 'gpt-6-astra'] as const;
type ModelId = typeof MODEL_IDS[number];
type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Obj = { [key: string]: Json };

export class ConvergenceFailure extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }
}

export interface ReviewedAccount {
  account_id: string;
  name: string;
  driver: 'openai-codex';
  auth_kind: 'oauth';
  connection_method: 'oauth';
  credential_generation: number;
  updated_at: number;
  astra: boolean;
}

export interface ReviewedRoute {
  route_id: string;
  public_model: ModelId;
  upstream_model: ModelId;
  protocol: 'openai';
  priority: number;
  enabled: true;
  updated_at: number;
  grant_revision: number;
  upstream_account_ids: string[];
  included_provider_group_ids: string[];
  excluded_provider_group_ids: string[];
  route_group_ids: string[];
  granted_credential_ids: string[];
  custom_model_confirmed: boolean;
}

export interface ReviewedManifest {
  schema_version: 1;
  tenant_external_id: string;
  target_base_url: string;
  accounts: ReviewedAccount[];
  routes: Record<ModelId, ReviewedRoute>;
}

export interface LiveAccount {
  id: string;
  tenant_external_id?: string;
  name: string;
  driver: string;
  auth_kind: string;
  connection_method: string;
  credential_generation: number;
  status: string;
  updated_at: number;
}

export interface LiveRoute {
  id: string;
  tenant_external_id?: string;
  public_model: string;
  upstream_model: string;
  protocol: string;
  priority: number;
  enabled: boolean;
  updated_at: number;
  grant_revision: number;
  upstream_account_ids: string[];
  included_provider_group_ids: string[];
  excluded_provider_group_ids: string[];
  route_group_ids: string[];
  granted_credential_ids: string[];
  custom_model_confirmed: boolean;
}

export interface AccountCatalog {
  account_id: string;
  status: string;
  credential_generation: number;
  models: Array<{ id: string; protocol: string }>;
}

export interface RoutePlan {
  model: ModelId;
  outcome: 'change' | 'replay';
  route: LiveRoute;
  desired_account_ids: string[];
}

function fail(code: string, message: string): never { throw new ConvergenceFailure(code, message); }
function object(value: Json | undefined, label: string): Obj {
  if (!value || Array.isArray(value) || typeof value !== 'object') fail('manifest_invalid', `${label} must be an object`);
  return value as Obj;
}
function exactKeys(value: Obj, expected: readonly string[], label: string): void {
  const actual = Object.keys(value).sort(); const wanted = [...expected].sort();
  if (!same(actual, wanted)) fail('manifest_invalid', `${label} fields do not match the schema`);
}
function text(value: Json | undefined, label: string): string {
  if (typeof value !== 'string' || value.trim() !== value || value.length === 0 || value.length > 500) fail('manifest_invalid', `${label} must be bounded non-empty text`);
  return value;
}
function integer(value: Json | undefined, label: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) fail('manifest_invalid', `${label} must be a non-negative safe integer`);
  return value;
}
function priority(value: Json | undefined): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < -1_000_000 || value > 1_000_000) fail('manifest_invalid', 'route priority is out of range');
  return value;
}
function uuid(value: Json | undefined, label: string): string {
  const parsed = text(value, label);
  if (!UUID.test(parsed)) fail('manifest_invalid', `${label} must be a lowercase UUID`);
  return parsed;
}
function bool(value: Json | undefined, label: string): boolean {
  if (typeof value !== 'boolean') fail('manifest_invalid', `${label} must be boolean`);
  return value;
}
function idList(value: Json | undefined, label: string): string[] {
  if (!Array.isArray(value) || value.length > 500) fail('manifest_invalid', `${label} must be a bounded array`);
  const parsed = value.map((item, index) => uuid(item, `${label}[${index}]`));
  if (!same(parsed, sorted(parsed))) fail('manifest_invalid', `${label} must be unique and sorted`);
  return parsed;
}
function sorted(values: readonly string[]): string[] { return [...new Set(values)].sort(); }
function ordered(values: readonly string[]): string[] { return [...values].sort(); }
function same(left: readonly string[], right: readonly string[]): boolean { return left.length === right.length && left.every((value, index) => value === right[index]); }
function canonical(value: Json): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key]!)}`).join(',')}}`;
  const encoded = JSON.stringify(value);
  if (encoded === undefined) fail('manifest_invalid', 'manifest contains a non-JSON value');
  return encoded;
}

function parseAccount(value: Json, index: number): ReviewedAccount {
  const item = object(value, `accounts[${index}]`);
  exactKeys(item, ['account_id', 'name', 'driver', 'auth_kind', 'connection_method', 'credential_generation', 'updated_at', 'astra'], `accounts[${index}]`);
  const driver = text(item.driver, 'account driver'); const auth = text(item.auth_kind, 'account auth_kind'); const method = text(item.connection_method, 'account connection_method');
  if (driver !== 'openai-codex' || auth !== 'oauth' || method !== 'oauth') fail('manifest_invalid', 'reviewed accounts must be native Codex OAuth accounts');
  return { account_id: uuid(item.account_id, 'account_id'), name: text(item.name, 'account name'), driver, auth_kind: auth, connection_method: method, credential_generation: integer(item.credential_generation, 'credential_generation'), updated_at: integer(item.updated_at, 'account updated_at'), astra: bool(item.astra, 'account astra') } as ReviewedAccount;
}

function parseRoute(value: Json, model: ModelId): ReviewedRoute {
  const item = object(value, `routes.${model}`);
  exactKeys(item, ['route_id', 'public_model', 'upstream_model', 'protocol', 'priority', 'enabled', 'updated_at', 'grant_revision', 'upstream_account_ids', 'included_provider_group_ids', 'excluded_provider_group_ids', 'route_group_ids', 'granted_credential_ids', 'custom_model_confirmed'], `routes.${model}`);
  if (text(item.public_model, 'public_model') !== model || text(item.upstream_model, 'upstream_model') !== model || text(item.protocol, 'protocol') !== 'openai' || item.enabled !== true) fail('manifest_invalid', `route ${model} must pin the enabled OpenAI ${model} identity`);
  return { route_id: uuid(item.route_id, 'route_id'), public_model: model, upstream_model: model, protocol: 'openai', priority: priority(item.priority), enabled: true, updated_at: integer(item.updated_at, 'route updated_at'), grant_revision: integer(item.grant_revision, 'grant_revision'), upstream_account_ids: idList(item.upstream_account_ids, 'upstream_account_ids'), included_provider_group_ids: idList(item.included_provider_group_ids, 'included_provider_group_ids'), excluded_provider_group_ids: idList(item.excluded_provider_group_ids, 'excluded_provider_group_ids'), route_group_ids: idList(item.route_group_ids, 'route_group_ids'), granted_credential_ids: idList(item.granted_credential_ids, 'granted_credential_ids'), custom_model_confirmed: bool(item.custom_model_confirmed, 'custom_model_confirmed') };
}

export function parseManifest(value: Json): ReviewedManifest {
  const root = object(value, 'manifest');
  exactKeys(root, ['schema_version', 'tenant_external_id', 'target_base_url', 'accounts', 'routes'], 'manifest');
  if (root.schema_version !== 1 || !Array.isArray(root.accounts) || root.accounts.length === 0 || root.accounts.length > 100) fail('manifest_invalid', 'unsupported schema version or account count');
  const tenant = text(root.tenant_external_id, 'tenant_external_id');
  let target: URL;
  try { target = new URL(text(root.target_base_url, 'target_base_url')); }
  catch { fail('manifest_invalid', 'target_base_url must be an absolute URL'); }
  if (target.username || target.password || target.search || target.hash || target.pathname !== '/' || (target.protocol !== 'https:' && !(target.protocol === 'http:' && ['127.0.0.1', 'localhost', '::1'].includes(target.hostname)))) fail('manifest_invalid', 'target_base_url must be an HTTPS origin (HTTP loopback is allowed for tests)');
  const accounts = root.accounts.map(parseAccount).sort((left, right) => left.account_id.localeCompare(right.account_id));
  if (new Set(accounts.map(item => item.account_id)).size !== accounts.length || new Set(accounts.map(item => item.name)).size !== accounts.length) fail('manifest_invalid', 'reviewed account IDs and names must be unique');
  if (accounts.filter(item => item.astra).length !== ASTRA_ACCOUNT_COUNT) fail('astra_account_set_invalid', 'Astra requires exactly four accounts selected by the private reviewed manifest');
  const routeRoot = object(root.routes, 'routes'); exactKeys(routeRoot, MODEL_IDS, 'routes');
  const routes = Object.fromEntries(MODEL_IDS.map(model => [model, parseRoute(routeRoot[model]!, model)])) as Record<ModelId, ReviewedRoute>;
  if (new Set(MODEL_IDS.map(model => routes[model].route_id)).size !== MODEL_IDS.length) fail('manifest_invalid', 'route IDs must be unique');
  return { schema_version: 1, tenant_external_id: tenant, target_base_url: target.href, accounts, routes };
}

function accountMatches(reviewed: ReviewedAccount, live: LiveAccount): boolean {
  return live.id === reviewed.account_id && live.name === reviewed.name && live.driver === reviewed.driver && live.auth_kind === reviewed.auth_kind && live.connection_method === reviewed.connection_method && live.credential_generation === reviewed.credential_generation && live.updated_at === reviewed.updated_at && live.status === 'active';
}

function routeIdentityMatches(reviewed: ReviewedRoute, live: LiveRoute): boolean {
  return live.id === reviewed.route_id && live.public_model === reviewed.public_model && live.upstream_model === reviewed.upstream_model && live.protocol === reviewed.protocol && live.priority === reviewed.priority && live.enabled === reviewed.enabled;
}
function routeRelationsMatch(reviewed: ReviewedRoute, live: LiveRoute): boolean {
  return same(ordered(live.upstream_account_ids), reviewed.upstream_account_ids) && same(ordered(live.included_provider_group_ids), reviewed.included_provider_group_ids) && same(ordered(live.excluded_provider_group_ids), reviewed.excluded_provider_group_ids) && same(ordered(live.route_group_ids), reviewed.route_group_ids) && same(ordered(live.granted_credential_ids), reviewed.granted_credential_ids) && live.custom_model_confirmed === reviewed.custom_model_confirmed;
}

export function buildPlan(manifest: ReviewedManifest, accounts: LiveAccount[], routes: LiveRoute[], catalogs: Map<string, AccountCatalog>): RoutePlan[] {
  const activeNative = accounts.filter(item => item.status === 'active' && item.driver === 'openai-codex' && item.auth_kind === 'oauth' && item.connection_method === 'oauth');
  if (!same(ordered(activeNative.map(item => item.id)), manifest.accounts.map(item => item.account_id))) fail('native_account_set_changed', 'active native Codex OAuth account set differs from the reviewed manifest');
  for (const reviewed of manifest.accounts) {
    const live = accounts.find(item => item.id === reviewed.account_id);
    if (!live || live.tenant_external_id !== manifest.tenant_external_id || !accountMatches(reviewed, live)) fail('account_cas_mismatch', `reviewed account ${reviewed.account_id} changed or is unavailable`);
  }
  for (const model of MODEL_IDS) {
    const matching = routes.filter(item => item.tenant_external_id === manifest.tenant_external_id && item.enabled && item.protocol === 'openai' && item.public_model === model);
    if (matching.length !== 1 || matching[0]!.id !== manifest.routes[model].route_id) fail('route_set_changed', `enabled OpenAI ${model} route set differs from the reviewed manifest`);
  }
  const allAccountIds = manifest.accounts.map(item => item.account_id);
  const astraAccountIds = manifest.accounts.filter(item => item.astra).map(item => item.account_id);
  return MODEL_IDS.map(model => {
    const reviewed = manifest.routes[model]; const live = routes.find(item => item.id === reviewed.route_id);
    if (!live || live.tenant_external_id !== manifest.tenant_external_id || !routeIdentityMatches(reviewed, live)) fail('route_cas_mismatch', `reviewed ${model} route changed or is unavailable`);
    const desired = model === 'gpt-6-astra' ? astraAccountIds : allAccountIds;
    for (const accountId of desired) {
      const account = manifest.accounts.find(item => item.account_id === accountId)!; const catalog = catalogs.get(accountId);
      if (!catalog || catalog.account_id !== accountId || catalog.credential_generation !== account.credential_generation || !['ready', 'stale'].includes(catalog.status) || !catalog.models.some(item => item.id === model && (item.protocol === 'openai' || item.protocol === 'any'))) fail('catalog_not_routable', `${model} is not present in a trusted current-generation catalog for ${accountId}`);
    }
    const desiredRelations = same(ordered(live.upstream_account_ids), desired) && live.included_provider_group_ids.length === 0 && live.excluded_provider_group_ids.length === 0 && same(ordered(live.route_group_ids), reviewed.route_group_ids) && same(ordered(live.granted_credential_ids), reviewed.granted_credential_ids) && !live.custom_model_confirmed;
    if (desiredRelations) return { model, outcome: 'replay', route: live, desired_account_ids: desired };
    if (live.updated_at !== reviewed.updated_at || live.grant_revision !== reviewed.grant_revision || !routeRelationsMatch(reviewed, live)) fail('route_cas_mismatch', `reviewed ${model} route relations changed`);
    return { model, outcome: 'change', route: live, desired_account_ids: desired };
  });
}

export class ControlClient {
  private readonly baseUrl: string;
  private readonly token: string;

  constructor(baseUrl: string, token: string) {
    this.baseUrl = baseUrl;
    this.token = token;
  }
  private async request(path: string, init?: RequestInit): Promise<Json> {
    const response = await fetch(new URL(path, this.baseUrl), { ...init, headers: { authorization: `Bearer ${this.token}`, 'content-type': 'application/json', ...init?.headers } });
    const body = await response.json().catch(() => null) as Json;
    if (!response.ok) fail('control_request_failed', `${init?.method ?? 'GET'} ${path} returned HTTP ${response.status}`);
    return body;
  }
  private async pages(path: string): Promise<Json[]> {
    const values: Json[] = []; let cursor = '';
    for (let page = 0; page < 100; page += 1) {
      const separator = path.includes('?') ? '&' : '?'; const body = await this.request(`${path}${cursor ? `${separator}${cursor}` : ''}`);
      if (!Array.isArray(body)) fail('control_response_invalid', `${path} did not return an array`);
      values.push(...body); if (body.length < 100) return values;
      const last = object(body.at(-1), `${path} cursor`); cursor = `before_created_at=${integer(last.created_at, 'created_at')}&before_id=${uuid(last.id, 'id')}`;
    }
    fail('control_response_invalid', `${path} exceeded the pagination bound`);
  }
  async accounts(tenant: string): Promise<LiveAccount[]> { return await this.pages(`/internal/v1/upstreams?tenant_external_id=${encodeURIComponent(tenant)}&limit=100`) as unknown as LiveAccount[]; }
  async routes(tenant: string): Promise<LiveRoute[]> { return await this.pages(`/internal/v1/model-routes?tenant_external_id=${encodeURIComponent(tenant)}&limit=100`) as unknown as LiveRoute[]; }
  async catalog(tenant: string, accountId: string): Promise<AccountCatalog> { return await this.request(`/internal/v1/upstreams/${accountId}/models?tenant_external_id=${encodeURIComponent(tenant)}&limit=200`) as unknown as AccountCatalog; }
  async update(tenant: string, item: RoutePlan): Promise<LiveRoute> {
    const route = item.route;
    return await this.request(`/internal/v1/model-routes/${route.id}`, { method: 'PUT', body: JSON.stringify({ tenant_external_id: tenant, public_model: route.public_model, upstream_model: route.upstream_model, protocol: route.protocol, priority: route.priority, upstream_account_ids: item.desired_account_ids, included_provider_group_ids: [], excluded_provider_group_ids: [], route_group_ids: route.route_group_ids, route_group_names: [], granted_credential_ids: route.granted_credential_ids, custom_model_confirmed: false, expected_updated_at: route.updated_at, expected_grant_revision: route.grant_revision }) }) as unknown as LiveRoute;
  }
}

export interface ControlApi {
  accounts(tenant: string): Promise<LiveAccount[]>;
  routes(tenant: string): Promise<LiveRoute[]>;
  catalog(tenant: string, accountId: string): Promise<AccountCatalog>;
  update(tenant: string, item: RoutePlan): Promise<LiveRoute>;
}

export async function converge(manifest: ReviewedManifest, client: ControlApi, apply: boolean): Promise<Obj> {
  const [accounts, routes] = await Promise.all([client.accounts(manifest.tenant_external_id), client.routes(manifest.tenant_external_id)]);
  const catalogs = new Map<string, AccountCatalog>();
  await Promise.all(manifest.accounts.map(async account => catalogs.set(account.account_id, await client.catalog(manifest.tenant_external_id, account.account_id))));
  const plan = buildPlan(manifest, accounts, routes, catalogs); let changed = 0;
  if (apply) for (const item of plan) if (item.outcome === 'change') {
    const updated = await client.update(manifest.tenant_external_id, item); const reviewed = manifest.routes[item.model];
    if (updated.tenant_external_id !== manifest.tenant_external_id || !routeIdentityMatches(reviewed, updated) || updated.updated_at <= item.route.updated_at || updated.grant_revision !== item.route.grant_revision || !same(ordered(updated.upstream_account_ids), item.desired_account_ids) || updated.included_provider_group_ids.length || updated.excluded_provider_group_ids.length || !same(ordered(updated.route_group_ids), reviewed.route_group_ids) || !same(ordered(updated.granted_credential_ids), reviewed.granted_credential_ids) || updated.custom_model_confirmed) fail('apply_verification_failed', `${item.model} update response did not match the reviewed target`);
    changed += 1;
  }
  return { mode: apply ? 'apply' : 'dry-run', manifest_sha256: manifestSha256(manifest), active_native_codex_account_count: manifest.accounts.length, astra_account_count: manifest.accounts.filter(item => item.astra).length, planned_change_count: plan.filter(item => item.outcome === 'change').length, replay_count: plan.filter(item => item.outcome === 'replay').length, changed_count: changed };
}

export function manifestSha256(manifest: ReviewedManifest): string {
  return createHash('sha256').update(canonical(manifest as unknown as Json)).digest('hex');
}

async function main(): Promise<void> {
  const args = process.argv.slice(2); const manifestPath = args[0]; const apply = args.length === 4 && args[1] === '--apply' && args[2] === '--approve';
  if (!manifestPath || (args.length !== 1 && !apply)) fail('usage', 'usage: node converge-codex-model-entitlements.ts MANIFEST [--apply --approve SHA256]');
  const manifest = parseManifest(JSON.parse(await readFile(manifestPath, 'utf8')) as Json);
  const digest = manifestSha256(manifest);
  if (apply && args[3] !== digest) fail('approval_required', `apply requires --approve ${digest}`);
  const token = process.env.MTC_SERVICE_TOKEN; const base = process.env.MTC_CONTROL_BASE_URL;
  if (!token || !base || new URL(base).href !== manifest.target_base_url) fail('environment_invalid', 'MTC_SERVICE_TOKEN and exact manifest MTC_CONTROL_BASE_URL are required');
  process.stdout.write(`${JSON.stringify(await converge(manifest, new ControlClient(base, token), apply))}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main().catch(error => { const code = error instanceof ConvergenceFailure ? error.code : 'unexpected'; process.stderr.write(`${JSON.stringify({ error: code, message: error instanceof Error ? error.message : 'unexpected failure' })}\n`); process.exitCode = 1; });
