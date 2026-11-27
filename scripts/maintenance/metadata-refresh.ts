/** Read cached metadata and render private evidence. Never runs inference.
 * Live: node metadata-refresh.ts --state-dir STATE --base-url MANAGEMENT_URL
 *       --context CONTEXT --namespace NAMESPACE --secret SECRET
 * Offline: node metadata-refresh.ts --state-dir STATE --snapshot SNAPSHOT
 * Outputs remain private; latest.json atomically selects a complete generation.
 */
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, openSync, closeSync, writeFileSync, renameSync,
  unlinkSync, existsSync, readFileSync, statSync } from 'node:fs';
import { basename, dirname, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';
import { randomUUID } from 'node:crypto';

type Row = Record<string, any>;
type Reader = (path: string, params: Row) => Promise<any>;
export const ACCOUNT_FIELDS = ['id', 'name', 'driver', 'status', 'route_count', 'created_at'];
export const ROUTE_FIELDS = ['id', 'public_model', 'upstream_model', 'protocol', 'enabled',
  'upstream_account_id', 'upstream_account_ids', 'candidate_upstream_account_ids'];
const pick = (row: Row, fields: readonly string[]) => {
  if (row === null || typeof row !== 'object' || Array.isArray(row)) throw new TypeError('metadata row must be an object');
  return Object.fromEntries(fields.map(key => [key, row[key] ?? null]));
};
const required = (row: Row, key: string) => {
  if (!Object.hasOwn(row, key)) throw new TypeError('required metadata field is absent');
  return row[key];
};
class MetadataReadError extends Error {
  constructor(code: string) { super('metadata read failed'); this.name = code; }
}
const optional = (row: Row, key: string, fallback: any) => row[key] === undefined ? fallback : row[key];
const quoted = (value: string) => encodeURIComponent(value).replace(/[!'()*]/g, char => '%' + char.charCodeAt(0).toString(16).toUpperCase());
const now = () => new Date().toISOString();
const load = (path: string) => JSON.parse(readFileSync(path, 'utf8'));
const identity = (...parts: unknown[]) => JSON.stringify(parts);
// Python's string ordering compares Unicode code points rather than UTF-16 units.
const compare = (a: string, b: string) => {
  const left = Array.from(a, c => c.codePointAt(0)!);
  const right = Array.from(b, c => c.codePointAt(0)!);
  for (let i = 0; i < Math.min(left.length, right.length); i++) {
    if (left[i] !== right[i]) return left[i] - right[i];
  }
  return left.length - right.length;
};
const sorted = (values: Iterable<string>) => Array.from(values).sort(compare);

export async function readPages(read: Reader, path: string, fields: readonly string[]): Promise<[Row[], number]> {
  const rows: Row[] = [], seen = new Set<unknown>();
  let cursor: Row = {}, pages = 0;
  while (true) {
    const response = await read(path, { limit: 100, ...cursor });
    const page = Array.isArray(response) ? response : response?.items;
    if (!Array.isArray(page)) throw new TypeError('metadata page must be an array');
    pages++;
    if (!page.length) return [rows, pages];
    for (const row of page) {
      const selected = pick(row, fields);
      required(row, 'id');
      if (seen.has(row.id)) throw new Error('pagination repeated an ID; snapshot not published');
      seen.add(row.id);
      rows.push(selected);
    }
    cursor = { before_created_at: required(page.at(-1), 'created_at'), before_id: required(page.at(-1), 'id') };
  }
}

export function privateWrite(path: string, value: unknown) {
  const temporary = join(dirname(path), `.${basename(path)}-${randomUUID()}`);
  let descriptor: number | undefined;
  try {
    descriptor = openSync(temporary, 'wx', 0o600);
    writeFileSync(descriptor, typeof value === 'string' ? value : JSON.stringify(value, null, 2));
    closeSync(descriptor); descriptor = undefined;
    renameSync(temporary, path);
  } finally {
    if (descriptor !== undefined) closeSync(descriptor);
    if (existsSync(temporary)) unlinkSync(temporary);
  }
}

export function managementUrl(raw: string) {
  const base = new URL(raw);
  if (!['http:', 'https:'].includes(base.protocol) || base.username || base.password || base.search || base.hash)
    throw new Error('base URL must be a clean management URL');
  if (base.protocol === 'http:' && !['127.0.0.1', 'localhost', '[::1]'].includes(base.hostname))
    throw new Error('HTTP management access requires loopback forwarding');
  if (!base.pathname.endsWith('/internal/v1/')) throw new Error('base URL must end in /internal/v1/');
  return base;
}

export async function collect(args: Row) {
  managementUrl(args['base-url']);
  const command = spawnSync('kubectl', ['--context', args.context, '-n', args.namespace,
    'get', 'secret', args.secret, '-o', 'json'], { encoding: 'utf8', shell: false, stdio: ['ignore', 'pipe', 'ignore'] });
  if (command.error || command.status !== 0) throw new Error('credential lookup failed');
  const secret = JSON.parse(command.stdout);
  let token: string | undefined = Buffer.from(secret.data[args['secret-key']], 'base64').toString('utf8');
  try { return await collectMetadata(metadataReader(args['base-url'], token, args.tenant)); }
  finally { token = undefined; }
}

export function metadataReader(base: string, token: string, tenant: string, fetcher = fetch): Reader {
  return async (path, params) => {
    const url = new URL(base + path);
    for (const [key, value] of Object.entries({ ...params, tenant_external_id: tenant })) url.searchParams.set(key, String(value));
    let response: Response;
    try {
      response = await fetcher(url, { method: 'GET', redirect: 'error',
        headers: { Authorization: 'Bearer ' + token }, signal: AbortSignal.timeout(15_000) });
    } catch (error) {
      throw new MetadataReadError(error instanceof Error && ['TimeoutError', 'AbortError'].includes(error.name) ? 'TimeoutError' : 'URLError');
    }
    if (!response.ok) throw new MetadataReadError('HTTPError');
    try { return await response.json(); }
    catch (error) {
      const code = error instanceof SyntaxError ? 'JSONDecodeError'
        : error instanceof Error && ['TimeoutError', 'AbortError'].includes(error.name) ? 'TimeoutError' : 'URLError';
      throw new MetadataReadError(code);
    }
  };
}

export async function collectMetadata(read: Reader) {
  const started = now();
  const [accounts, accountPages] = await readPages(read, 'upstreams', ACCOUNT_FIELDS);
  const [routes, pages] = await readPages(read, 'model-routes', ROUTE_FIELDS);
  const result: Row[] = [];
  for (const account of accounts) {
    const item = pick(account, ['id', 'name', 'driver', 'status', 'route_count']);
    try {
      const catalog = await read('upstreams/' + quoted(account.id) + '/models', { limit: 10000 });
      item.catalog = pick(catalog, ['status', 'credential_generation', 'last_attempt_at', 'last_success_at', 'expires_at', 'error_code']);
      for (const key of ['models', 'disabled_models']) item.catalog[key] = optional(catalog, key, []).map((model: Row) => pick(model, ['id', 'protocol']));
      item.catalog_read = 'success';
    } catch (error) {
      if (!(error instanceof MetadataReadError)) throw error;
      item.catalog_read = 'failed';
      item.error_type = error instanceof Error ? error.name : 'Error';
    }
    result.push(item);
  }
  return { schema_version: '0.3', collection_started_at: started, metadata_updated_at: now(),
    account_pagination_complete: true, account_pages_including_empty_terminal: accountPages,
    route_pagination_complete: true, route_pages_including_empty_terminal: pages,
    route_count: routes.length, accounts: result, routes,
    consistency: 'sequential API observations, not a database transaction snapshot' };
}

export function render(snapshot: Row, canonical: Row, evidence: Row) {
  const byIdentity = new Map<string, string[]>(), byChannelAlias = new Map<string, string[]>();
  const accountChannels = new Map<string, Set<string>>(), historical = new Map<string, Row[]>();
  const append = <T>(map: Map<string, T[]>, key: string, value: T) => map.set(key, [...(map.get(key) ?? []), value]);
  const channel = (account: string, value: string) => {
    if (!accountChannels.has(account)) accountChannels.set(account, new Set());
    accountChannels.get(account)!.add(value);
  };
  for (const record of canonical.records) {
    if (record.account_id) channel(record.account_id, record.channel_id);
    for (const alias of new Set([record.model_id, record.catalog_model_id].filter(value => typeof value === 'string'))) {
      append(byChannelAlias, identity(record.channel_id, alias), record.record_id);
      if (record.account_id) append(byIdentity, identity(record.account_id, alias), record.record_id);
    }
  }
  for (const [account, value] of Object.entries(evidence.channel_bindings ?? {})) channel(account, value as string);
  const domestic = new Set(evidence.domestic_provider_account_ids ?? []);
  for (const entry of evidence.entries ?? []) append(historical, identity(entry.account_id, entry.model_id, entry.protocol), entry);
  const rows: Row[] = [], accounts: Row[] = [];
  for (const account of snapshot.accounts) {
    const catalog = optional(account, 'catalog', {}), channels = sorted(accountChannels.get(account.id) ?? []);
    const scope = domestic.has(account.id) ? 'domestic_provider' : 'unknown_provider_scope';
    const summary = { ...account }; delete summary.catalog;
    accounts.push({ ...summary, channel_ids: channels, provider_scope: scope,
      catalog_metadata: Object.fromEntries(Object.entries(catalog).filter(([key]) => !['models', 'disabled_models'].includes(key))),
      catalog_count: optional(catalog, 'models', []).length, disabled_count: optional(catalog, 'disabled_models', []).length,
      catalog_coverage: account.catalog_read !== 'success' || ['models', 'disabled_models'].some(key => optional(catalog, key, []).length >= 10000)
        ? 'unknown' : 'cached_snapshot_complete' });
    const models = new Map<string, Row>();
    const getModel = (alias: string, protocol: string) => {
      const key = identity(alias, protocol);
      if (!models.has(key)) models.set(key, { alias, protocol });
      return models.get(key)!;
    };
    for (const model of optional(catalog, 'models', [])) getModel(model.id, model.protocol).catalog_present = true;
    for (const model of optional(catalog, 'disabled_models', [])) getModel(model.id, model.protocol).catalog_disabled = true;
    for (const route of snapshot.routes) {
      const configured = new Set([...(route.upstream_account_ids || []), route.upstream_account_id]);
      const candidates = new Set(route.candidate_upstream_account_ids || []);
      if (!configured.has(account.id) && !candidates.has(account.id)) continue;
      const model = getModel(route.upstream_model, route.protocol);
      (model.configured_routes ??= []).push(pick(route, ['id', 'public_model', 'enabled']));
      model.configured = true;
      model.explicit_account_binding ||= configured.has(account.id);
      model.candidate_membership ||= candidates.has(account.id);
    }
    for (const model of Array.from(models.values()).sort((a, b) => compare(a.alias, b.alias) || compare(a.protocol, b.protocol))) {
      const { alias, protocol } = model;
      const records = new Set(byIdentity.get(identity(account.id, alias)) ?? []);
      for (const value of channels) for (const id of byChannelAlias.get(identity(value, alias)) ?? []) records.add(id);
      const history = [...(historical.get(identity(account.id, alias, protocol)) ?? [])].sort((a, b) => compare(a.tested_at, b.tested_at));
      const latest = history.at(-1);
      rows.push({ account_id: account.id, account_name: account.name, model_id: alias, protocol,
        provider_scope: scope, model_origin: 'unknown', canonical_record_ids: sorted(records),
        canonical_mapping: records.size ? 'exact_account_or_reviewed_channel_alias' : 'unmapped',
        metadata_updated_at: snapshot.metadata_updated_at, catalog_present: model.catalog_present ?? false,
        catalog_supports_protocol: Boolean(model.catalog_present || models.get(identity(alias, 'any'))?.catalog_present),
        catalog_disabled: model.catalog_disabled ?? false, configured: model.configured ?? false,
        explicit_account_binding: model.explicit_account_binding ?? false, candidate_membership: model.candidate_membership ?? false,
        configured_routes: model.configured_routes ?? [], verification_status: latest?.outcome ?? 'unknown',
        tested_at: latest?.tested_at ?? null, verification_evidence: history });
    }
  }
  const matched = new Set(rows.map(row => identity(row.account_id, row.model_id, row.protocol)));
  return { schema_version: '0.3', rendered_at: now(), metadata_updated_at: snapshot.metadata_updated_at,
    route_count: snapshot.route_count, route_pagination_complete: snapshot.route_pagination_complete,
    account_pagination_complete: snapshot.account_pagination_complete ?? false,
    account_pages_including_empty_terminal: snapshot.account_pages_including_empty_terminal ?? null,
    accounts, models: rows, canonical_directory_records: canonical.records.length,
    unmatched_historical_evidence: Array.from(historical).filter(([key]) => !matched.has(key)).flatMap(([, entries]) => entries),
    notes: ['cached catalog presence does not prove callable', 'provider scope does not determine model origin',
      'configured includes candidates and does not prove caller grants', 'historical evidence is exact identity only',
      'canonical test_evidence preserved in source, not inferred from incompatible legacy protocol fields'] };
}

export function markdown(report: Row) {
  const lines = ['# 国产上游模型目录与验证状态', '', '元数据更新时间：' + report.metadata_updated_at,
    '', '完整路由：' + report.route_count + '；账号/模型/协议条目：' + report.models.length,
    '', 'catalog 表示缓存记录；configured 表示配置或候选关联；精确实测及其时间单独记录。',
    '国产上游包含聚合转发，模型产地保持未知。映射沿用原采购目录，未匹配条目保留 unmapped。', '',
    '|上游|账号状态|缓存状态|目录条目|已移除|覆盖|', '|---|---|---|---:|---:|---|'];
  const cell = (value: unknown) => String(value).replaceAll('|', '\\|').replaceAll('\n', ' ').replaceAll('\r', ' ');
  for (const account of report.accounts) lines.push('|' + [account.name, account.status, account.catalog_metadata.status ?? 'unknown',
    account.catalog_count, account.disabled_count, account.catalog_coverage].map(cell).join('|') + '|');
  lines.push('', '完整精确别名、原采购 record_id、协议、历史证据见同代 report.json。',
    '历史证据与原采购记录保持独立，刷新元数据不会重置 tested_at。',
    '本入口只执行 GET 元数据读取；未执行模型生成、路由修改、部署或业务验收。',
    '模型测试需在指定测试站上线及容量就绪后单独执行。');
  return lines.join('\n') + '\n';
}

export function publish(outputDir: string, snapshot: Row, report: Row, write = privateWrite) {
  const generation = mkdtempSync(join(outputDir, 'snapshot-'));
  write(join(generation, 'metadata.json'), snapshot);
  write(join(generation, 'report.json'), report);
  write(join(generation, 'report.md'), markdown(report));
  write(join(outputDir, 'latest.json'), { generation: basename(generation), metadata_updated_at: report.metadata_updated_at, rendered_at: report.rendered_at });
  return generation;
}

class CliArgumentError extends Error {}
export async function main(argv = process.argv.slice(2)) {
  if (argv.includes('--help') || argv.includes('-h')) {
    console.log('Usage: node metadata-refresh.ts --state-dir STATE [--snapshot SNAPSHOT | --base-url URL --context CONTEXT --namespace NAMESPACE --secret SECRET]\nOptions: --secret-key service-token --tenant default --canonical PATH --evidence PATH --output-dir PATH');
    return;
  }
  const names = ['snapshot', 'base-url', 'context', 'namespace', 'secret', 'secret-key', 'tenant', 'state-dir', 'canonical', 'evidence', 'output-dir'];
  let args: Record<string, string | undefined>;
  try {
    args = parseArgs({ args: argv, options: Object.fromEntries(names.map(name => [name, { type: 'string' as const }])) }).values as Record<string, string | undefined>;
    if (!args['state-dir']) throw new Error('--state-dir is required');
    if (!args.snapshot && !['base-url', 'context', 'namespace', 'secret'].every(key => args[key]))
      throw new Error('live refresh requires --base-url --context --namespace --secret');
  } catch { throw new CliArgumentError(); }
  args['secret-key'] ??= 'service-token'; args.tenant ??= 'default';
  args.canonical ??= join(args['state-dir'], 'canonical.json');
  args.evidence ??= join(args['state-dir'], 'evidence.json');
  args['output-dir'] ??= join(args['state-dir'], 'metadata-private');
  process.umask(0o077);
  mkdirSync(args['output-dir'], { mode: 0o700, recursive: true });
  if (statSync(args['output-dir']).mode & 0o077) throw new Error('output directory must be private (0700)');
  const snapshot = args.snapshot ? load(args.snapshot) : await collect(args);
  const report = render(snapshot, load(args.canonical), load(args.evidence));
  const generation = publish(args['output-dir'], snapshot, report);
  console.log(JSON.stringify({ generation: basename(generation), metadata_updated_at: report.metadata_updated_at,
    route_count: report.route_count, model_entries: report.models.length }));
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => {
    console.log(JSON.stringify({ status: 'failed', error_type: error instanceof Error ? error.name : 'Error' }));
    process.exitCode = error instanceof CliArgumentError ? 2 : 1;
  });
}
