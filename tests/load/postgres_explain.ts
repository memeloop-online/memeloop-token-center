#!/usr/bin/env node
/** Read-only PostgreSQL plan/latency benchmark for high-volume observability queries. */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SAFE_ID = /^[0-9A-Fa-f-]{36}$/;
const LARGE_HISTORY_RELATIONS = ["request_records", "request_events", "request_stats_facts", "request_daily_aggregates", "generation_jobs", "generation_stats_facts", "generation_daily_aggregates", "usage_analysis_hourly", "usage_analysis_daily"];
const LIBPQ_QUERY_ENV: Record<string, string> = {
  application_name: "PGAPPNAME", channel_binding: "PGCHANNELBINDING", client_encoding: "PGCLIENTENCODING", connect_timeout: "PGCONNECT_TIMEOUT", gssencmode: "PGGSSENCMODE", keepalives: "PGKEEPALIVES", keepalives_count: "PGKEEPALIVESCOUNT", keepalives_idle: "PGKEEPALIVESIDLE", keepalives_interval: "PGKEEPALIVESINTERVAL", options: "PGOPTIONS", passfile: "PGPASSFILE", requirepeer: "PGREQUIREPEER", sslcert: "PGSSLCERT", sslcrl: "PGSSLCRL", sslcrldir: "PGSSLCRLDIR", sslkey: "PGSSLKEY", sslmode: "PGSSLMODE", sslrootcert: "PGSSLROOTCERT", sslsni: "PGSSNI", ssl_max_protocol_version: "PGSSLMAXPROTOCOLVERSION", ssl_min_protocol_version: "PGSSLMINPROTOCOLVERSION", target_session_attrs: "PGTARGETSESSIONATTRS", tcp_user_timeout: "PGTCPUSER_TIMEOUT",
};

export class PrerequisiteFailure extends Error {}
type JsonObject = Record<string, any>;

interface Arguments { databaseUrlFile?: string; output?: string; maxExecutionMs: number; statementTimeoutMs: number; minRequestRows: number; maxSequentialScanRows: number; allowSequentialScan: boolean }

function numberValue(flag: string, value: string | undefined): number {
  if (value === undefined || !Number.isFinite(Number(value))) throw new PrerequisiteFailure(`${flag} requires a number`);
  return Number(value);
}

function parseArgs(argv: string[]): Arguments {
  const args: Arguments = { maxExecutionMs: 250, statementTimeoutMs: 30_000, minRequestRows: 100_000, maxSequentialScanRows: 10_000, allowSequentialScan: false };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--allow-sequential-scan") { args.allowSequentialScan = true; continue; }
    if (flag === "--help" || flag === "-h") { console.log("Usage: postgres_explain.ts [--database-url-file PATH] [--output PATH] [--max-execution-ms N] [--statement-timeout-ms N] [--min-request-rows N] [--max-sequential-scan-rows N] [--allow-sequential-scan]"); process.exit(0); }
    const value = argv[++index];
    if (flag === "--database-url-file") args.databaseUrlFile = value;
    else if (flag === "--output") args.output = value;
    else if (flag === "--max-execution-ms") args.maxExecutionMs = numberValue(flag, value);
    else if (flag === "--statement-timeout-ms") args.statementTimeoutMs = numberValue(flag, value);
    else if (flag === "--min-request-rows") args.minRequestRows = numberValue(flag, value);
    else if (flag === "--max-sequential-scan-rows") args.maxSequentialScanRows = numberValue(flag, value);
    else throw new PrerequisiteFailure(`unrecognized argument: ${flag}`);
  }
  return args;
}

function executableExists(name: string): boolean { return (process.env.PATH ?? "").split(":").some((directory) => existsSync(`${directory}/${name}`)); }

export function libpqEnvironment(databaseUrl: string): NodeJS.ProcessEnv {
  let parsed: URL;
  try { parsed = new URL(databaseUrl); } catch { throw new PrerequisiteFailure("database URL must use postgres:// or postgresql://"); }
  if (parsed.protocol !== "postgres:" && parsed.protocol !== "postgresql:") throw new PrerequisiteFailure("database URL must use postgres:// or postgresql://");
  if (!parsed.hostname) throw new PrerequisiteFailure("database URL must include a host");
  const environment = { ...process.env };
  for (const key of ["PGHOST", "PGPORT", "PGDATABASE", "PGUSER", "PGPASSWORD"]) delete environment[key];
  environment.PGHOST = decodeURIComponent(parsed.hostname);
  environment.PGDATABASE = decodeURIComponent(parsed.pathname.replace(/^\//u, ""));
  if (!environment.PGDATABASE) throw new PrerequisiteFailure("database URL must include a database name");
  if (parsed.port) environment.PGPORT = parsed.port;
  if (parsed.username) environment.PGUSER = decodeURIComponent(parsed.username);
  if (parsed.password) environment.PGPASSWORD = decodeURIComponent(parsed.password);
  for (const [key, value] of parsed.searchParams) {
    const envKey = LIBPQ_QUERY_ENV[key];
    if (envKey === undefined) throw new PrerequisiteFailure(`unsupported PostgreSQL URL option: ${key}`);
    environment[envKey] = value;
  }
  return environment;
}

export function runPsql(databaseUrl: string, sql: string, timeoutMs: number): string {
  const environment = libpqEnvironment(databaseUrl);
  environment.PGOPTIONS = `${environment.PGOPTIONS ?? ""} -c default_transaction_read_only=on -c statement_timeout=${timeoutMs}`.trim();
  const result = spawnSync("psql", ["-X", "-q", "-A", "-t", "--no-psqlrc"], { input: sql, encoding: "utf8", env: environment, timeout: Math.max(30_000, timeoutMs + 10_000) });
  if (result.error) throw new PrerequisiteFailure(result.error.message);
  if (result.status !== 0) throw new PrerequisiteFailure((result.stderr ?? "").trim().split(/\r?\n/u).filter(Boolean).at(-1) ?? "unknown psql error");
  return (result.stdout ?? "").trim();
}

function scalar(databaseUrl: string, sql: string, timeoutMs: number): string { return runPsql(databaseUrl, sql, timeoutMs).split(/\r?\n/u).at(-1)?.trim() ?? ""; }
function literal(databaseUrl: string, sql: string, timeoutMs: number): string | undefined { return scalar(databaseUrl, `SELECT quote_literal(value) FROM (${sql}) q(value);`, timeoutMs) || undefined; }
function planNodes(plan: JsonObject): JsonObject[] { return [plan, ...(Array.isArray(plan.Plans) ? plan.Plans.flatMap((child: JsonObject) => planNodes(child)) : [])]; }

function explain(databaseUrl: string, name: string, query: string, timeoutMs: number, maxSequentialScanRows: number): JsonObject {
  const document = (JSON.parse(runPsql(databaseUrl, `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) ${query};`, timeoutMs)) as JsonObject[])[0]!;
  const plan = document.Plan as JsonObject;
  const nodes = planNodes(plan);
  const sequentialScans = nodes.flatMap((node) => {
    const relation = String(node["Relation Name"] ?? "");
    if (node["Node Type"] !== "Seq Scan" || !LARGE_HISTORY_RELATIONS.some((prefix) => relation.startsWith(prefix))) return [];
    const loops = Math.max(1, Number(node["Actual Loops"] ?? 1));
    return [{ relation, scanned_rows: (Number(node["Actual Rows"] ?? 0) + Number(node["Rows Removed by Filter"] ?? 0)) * loops }];
  });
  return {
    name, planning_time_ms: document["Planning Time"] ?? 0, execution_time_ms: document["Execution Time"] ?? 0,
    returned_rows: plan["Actual Rows"] ?? 0, root_node: plan["Node Type"],
    indexes: [...new Set(nodes.map((node) => node["Index Name"]).filter((value): value is string => value !== undefined).map(String))].sort(),
    relations: [...new Set(nodes.map((node) => node["Relation Name"]).filter((value): value is string => value !== undefined).map(String))].sort(),
    sequential_large_relations: sequentialScans.filter((scan) => scan.scanned_rows > maxSequentialScanRows).map((scan) => scan.relation).sort(),
    sequential_scans: sequentialScans,
    shared_hit_blocks: nodes.reduce((sum, node) => sum + Number(node["Shared Hit Blocks"] ?? 0), 0),
    shared_read_blocks: nodes.reduce((sum, node) => sum + Number(node["Shared Read Blocks"] ?? 0), 0), plan,
  };
}

function prerequisite(error: unknown): number { console.error(JSON.stringify({ passed: false, exit_code: 3, error_kind: "prerequisite", error: error instanceof Error ? error.message : String(error) })); return 3; }
function parseBounds(raw: string, absent: string): [number, number] {
  const parts = raw.split("|").map(Number);
  if (parts.length !== 2 || parts.some((value) => !Number.isFinite(value))) throw new PrerequisiteFailure(absent);
  return parts as [number, number];
}

export function main(argv = process.argv.slice(2)): number {
  let args: Arguments;
  try { args = parseArgs(argv); } catch (error) { return prerequisite(error); }
  if (!executableExists("psql")) return prerequisite(new PrerequisiteFailure("psql is not installed"));
  let databaseUrl: string;
  try { databaseUrl = (args.databaseUrlFile ? readFileSync(args.databaseUrlFile, "utf8") : process.env.MTC_BENCH_DATABASE_URL ?? "").trim(); } catch (error) { return prerequisite(error); }
  if (!databaseUrl) return prerequisite(new PrerequisiteFailure("MTC_BENCH_DATABASE_URL or --database-url-file is required"));
  const repository = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
  const output = resolve(args.output ?? `${repository}/tests/load/results/postgres-explain-latest.json`);
  mkdirSync(dirname(output), { recursive: true });
  try {
    const timeout = args.statementTimeoutMs;
    const count = (sql: string): number => Number.parseInt(scalar(databaseUrl, sql, timeout), 10);
    const requestRows = count("SELECT count(*) FROM request_records;");
    const eventRows = count("SELECT count(*) FROM request_events;");
    const factRows = count("SELECT count(*) FROM request_stats_facts;");
    const generationFactRows = count("SELECT count(*) FROM generation_stats_facts;");
    const terminalRequestRows = count("SELECT count(*) FROM request_records WHERE completed_at IS NOT NULL AND status_code IS NOT NULL;");
    const terminalGenerationRows = count("SELECT count(*) FROM generation_jobs WHERE status IN ('succeeded', 'failed', 'cancelled');");
    const blankCurrencyRows = count("SELECT SUM(rows) FROM (SELECT count(*) AS rows FROM request_records WHERE currency = '' UNION ALL SELECT count(*) FROM request_stats_facts WHERE currency = '' UNION ALL SELECT count(*) FROM request_daily_aggregates WHERE currency = '' UNION ALL SELECT count(*) FROM generation_stats_facts WHERE currency = '' UNION ALL SELECT count(*) FROM generation_daily_aggregates WHERE currency = '' UNION ALL SELECT count(*) FROM usage_analysis_hourly WHERE currency = '' UNION ALL SELECT count(*) FROM usage_analysis_daily WHERE currency = '') currency_gaps;");
    const validRequiredCursorIndexes = count("SELECT count(*) FROM pg_index WHERE indexrelid IN (to_regclass('public.request_records_recent_idx'), to_regclass('public.request_events_global_cursor_idx')) AND indisvalid AND indisready;");
    const unattachedRequiredCursorLeaves = count("WITH required(parent_table, parent_index) AS (VALUES ('request_records', 'request_records_recent_idx'), ('request_events', 'request_events_global_cursor_idx')) SELECT count(*) FROM required r JOIN pg_inherits table_inheritance ON table_inheritance.inhparent = to_regclass('public.' || r.parent_table) WHERE NOT EXISTS (SELECT 1 FROM pg_inherits index_inheritance JOIN pg_index child_index ON child_index.indexrelid = index_inheritance.inhrelid WHERE index_inheritance.inhparent = to_regclass('public.' || r.parent_index) AND child_index.indrelid = table_inheritance.inhrelid AND child_index.indisvalid AND child_index.indisready);");
    const validRequiredObservabilityIndexes = count("SELECT count(*) FROM pg_index WHERE indexrelid IN (to_regclass('public.request_stats_facts_tenant_created_idx'), to_regclass('public.generation_stats_facts_tenant_created_idx'), to_regclass('public.request_daily_aggregates_tenant_day_idx'), to_regclass('public.generation_daily_aggregates_tenant_day_idx'), to_regclass('public.usage_analysis_hourly_tenant_time_idx'), to_regclass('public.usage_analysis_daily_tenant_time_idx'), to_regclass('public.usage_analysis_daily_tenant_model_time_idx'), to_regclass('public.usage_analysis_daily_tenant_error_time_idx'), to_regclass('public.usage_analysis_daily_tenant_route_time_idx')) AND indisvalid AND indisready;");
    const keyId = scalar(databaseUrl, "SELECT key_id FROM request_records GROUP BY key_id ORDER BY count(*) DESC LIMIT 1;", timeout);
    const tenantId = scalar(databaseUrl, "SELECT tenant_id FROM request_records GROUP BY tenant_id ORDER BY count(*) DESC LIMIT 1;", timeout);
    const eventTenantId = scalar(databaseUrl, "SELECT tenant_id FROM request_events GROUP BY tenant_id ORDER BY count(*) DESC LIMIT 1;", timeout);
    for (const [name, value] of [["key_id", keyId], ["tenant_id", tenantId]] as const) if (!SAFE_ID.test(value)) throw new PrerequisiteFailure(`sample ${name} is absent or malformed`);
    if (eventTenantId && !SAFE_ID.test(eventTenantId)) throw new PrerequisiteFailure("sample event tenant id is malformed");
    const errorCode = literal(databaseUrl, "SELECT error_code FROM request_records WHERE error_code IS NOT NULL GROUP BY error_code ORDER BY count(*) DESC LIMIT 1", timeout);
    const analysisModel = literal(databaseUrl, `SELECT model FROM usage_analysis_daily WHERE tenant_id = '${tenantId}' GROUP BY model ORDER BY count(*) DESC LIMIT 1`, timeout);
    const analysisError = literal(databaseUrl, `SELECT error_code FROM usage_analysis_daily WHERE tenant_id = '${tenantId}' AND error_code <> '' GROUP BY error_code ORDER BY count(*) DESC LIMIT 1`, timeout);
    const analysisRoute = literal(databaseUrl, `SELECT model_route_id FROM usage_analysis_daily WHERE tenant_id = '${tenantId}' AND model_route_id <> '' GROUP BY model_route_id ORDER BY count(*) DESC LIMIT 1`, timeout);
    const [statsMinDay, statsMaxDay] = parseBounds(scalar(databaseUrl, `SELECT min(day_bucket)::text || '|' || max(day_bucket)::text FROM request_daily_aggregates WHERE tenant_id = '${tenantId}';`, timeout), "request daily aggregates are absent for the sample tenant");
    const statsFrom = Math.max(statsMinDay, statsMaxDay - 92) * 86_400_000 + 1;
    const statsTo = (statsMaxDay + 1) * 86_400_000 - 2;
    const statsFullDayFrom = Math.ceil(statsFrom / 86_400_000) * 86_400_000;
    const statsFullDayTo = Math.ceil((statsTo + 1) / 86_400_000) * 86_400_000;
    const filteredStatsQuery = `WITH filtered_activity AS MATERIALIZED (SELECT day_bucket * 86400000 AS created_at, model, status_class, error_code, requests, input_tokens, output_tokens, cost_micros FROM request_daily_aggregates WHERE tenant_id = '${tenantId}' AND day_bucket >= ${statsFullDayFrom} / 86400000 AND day_bucket < ${statsFullDayTo} / 86400000 UNION ALL SELECT created_at, model, status_class, error_code, 1, input_tokens, output_tokens, cost_micros FROM request_stats_facts WHERE tenant_id = '${tenantId}' AND created_at >= ${statsFrom} AND created_at <= ${statsTo} AND (created_at < ${statsFullDayFrom} OR created_at >= ${statsFullDayTo})), enriched AS (SELECT model, created_at / 86400000 AS day_bucket, NULLIF(error_code, '') AS error_bucket, status_class, requests, input_tokens, output_tokens, cost_micros FROM filtered_activity) SELECT model, day_bucket, error_bucket, SUM(requests), SUM(input_tokens), SUM(output_tokens), SUM(cost_micros) FROM enriched GROUP BY GROUPING SETS ((), (model), (day_bucket), (error_bucket)) HAVING GROUPING(error_bucket) = 1 OR error_bucket IS NOT NULL`;
    const [usageMinDay, usageMaxDay] = parseBounds(scalar(databaseUrl, `SELECT min(day_bucket)::text || '|' || max(day_bucket)::text FROM usage_analysis_daily WHERE tenant_id = '${tenantId}';`, timeout), "usage analysis daily rollups are absent for the sample tenant");
    const usageFromDay = Math.max(usageMinDay, usageMaxDay - 92);
    const usageDailyQuery = `WITH filtered_activity AS MATERIALIZED (SELECT * FROM usage_analysis_daily WHERE tenant_id = '${tenantId}' AND day_bucket >= ${usageFromDay} AND day_bucket <= ${usageMaxDay}) SELECT currency, SUM(requests), SUM(input_tokens), SUM(cached_input_tokens), SUM(cache_write_tokens), SUM(generation_units), SUM(duration_sum_ms), SUM(cost_micros) FROM filtered_activity GROUP BY GROUPING SETS ((currency), (day_bucket, currency), (model, currency), (key_id, currency), (upstream_account_id, currency), (protocol, currency), (status_class, currency), (error_code, currency))`;
    const [usageMinHour, usageMaxHour] = parseBounds(scalar(databaseUrl, `SELECT min(hour_bucket)::text || '|' || max(hour_bucket)::text FROM usage_analysis_hourly WHERE tenant_id = '${tenantId}';`, timeout), "usage analysis hourly rollups are absent for the sample tenant");
    const usageFromHour = Math.max(usageMinHour, usageMaxHour - 31 * 24);
    const usageHourlyQuery = `SELECT hour_bucket, currency, protocol, SUM(requests), SUM(input_tokens), SUM(cached_input_tokens), SUM(cache_write_tokens), SUM(cost_micros) FROM usage_analysis_hourly WHERE tenant_id = '${tenantId}' AND hour_bucket >= ${usageFromHour} AND hour_bucket <= ${usageMaxHour} GROUP BY hour_bucket, currency, protocol ORDER BY hour_bucket, currency, protocol`;
    const queries: Array<[string, string]> = [
      ["global_newest_cursor", "SELECT id, created_at, model, status_code FROM request_records ORDER BY created_at DESC, id DESC LIMIT 100"],
      ["tenant_newest_cursor", `SELECT id, created_at, model, status_code FROM request_records WHERE tenant_id = '${tenantId}' ORDER BY created_at DESC, id DESC LIMIT 100`],
      ["key_newest_cursor", `SELECT id, created_at, model, status_code FROM request_records WHERE key_id = '${keyId}' ORDER BY created_at DESC, id DESC LIMIT 100`],
      ["key_daily_aggregate", `SELECT day_bucket, SUM(requests), SUM(input_tokens), SUM(output_tokens), SUM(cost_micros) FROM usage_daily_aggregates WHERE key_id = '${keyId}' GROUP BY day_bucket ORDER BY day_bucket`],
      ["tenant_filtered_stats", filteredStatsQuery], ["tenant_usage_analysis_daily_93d", usageDailyQuery], ["tenant_usage_analysis_hourly_31d", usageHourlyQuery],
    ];
    if (errorCode !== undefined) queries.push(["tenant_error_troubleshooting", `SELECT id, created_at, model, status_code, error_code FROM request_records WHERE tenant_id = '${tenantId}' AND error_code = ${errorCode} ORDER BY created_at DESC, id DESC LIMIT 100`]);
    for (const [name, column, value] of [["tenant_usage_model_drilldown", "model", analysisModel], ["tenant_usage_error_drilldown", "error_code", analysisError], ["tenant_usage_route_drilldown", "model_route_id", analysisRoute]] as const) if (value !== undefined) queries.push([name, `SELECT currency, SUM(requests), SUM(cost_micros) FROM usage_analysis_daily WHERE tenant_id = '${tenantId}' AND ${column} = ${value} AND day_bucket >= ${usageFromDay} AND day_bucket <= ${usageMaxDay} GROUP BY currency`]);
    if (eventTenantId) queries.push(["tenant_event_cursor", `SELECT event_id, event_at, request_id, event_kind FROM request_events WHERE tenant_id = '${eventTenantId}' ORDER BY event_at ASC, event_id ASC LIMIT 500`]);
    const results = queries.map(([name, query]) => explain(databaseUrl, name, query, timeout, args.maxSequentialScanRows));
    const checks: JsonObject[] = [
      { name: "large-volume request row precondition", actual: requestRows, operator: ">=", expected: args.minRequestRows, passed: requestRows >= args.minRequestRows },
      { name: "terminal request fact coverage", actual: factRows, operator: "==", expected: terminalRequestRows, passed: factRows === terminalRequestRows },
      { name: "terminal generation fact coverage", actual: generationFactRows, operator: "==", expected: terminalGenerationRows, passed: generationFactRows === terminalGenerationRows },
      { name: "historical billing currency is complete", actual: blankCurrencyRows, operator: "==", expected: 0, passed: blankCurrencyRows === 0 },
      { name: "required global cursor parent indexes are ready", actual: validRequiredCursorIndexes, operator: "==", expected: 2, passed: validRequiredCursorIndexes === 2 },
      { name: "required global cursor indexes cover every partition", actual: unattachedRequiredCursorLeaves, operator: "==", expected: 0, passed: unattachedRequiredCursorLeaves === 0 },
      { name: "required observability indexes are ready", actual: validRequiredObservabilityIndexes, operator: "==", expected: 9, passed: validRequiredObservabilityIndexes === 9 },
    ];
    for (const result of results) {
      checks.push({ name: `${result.name} execution time`, actual: result.execution_time_ms, operator: "<=", expected: args.maxExecutionMs, passed: result.execution_time_ms <= args.maxExecutionMs });
      if (!args.allowSequentialScan && requestRows >= args.minRequestRows) {
        const scans = result.sequential_large_relations as string[];
        const bounded = result.name === "tenant_filtered_stats" && scans.every((relation) => relation === "request_stats_facts" || relation === "generation_stats_facts");
        checks.push({ name: `${result.name} avoids sequential history scan`, actual: scans, operator: "==", expected: "[] except bounded incomplete-day fact branches", passed: scans.length === 0 || bounded });
      }
    }
    const passed = checks.every((item) => item.passed);
    const report = { schema_version: 3, benchmark: "memeloop-token-center-postgres-explain", generated_at: new Date().toISOString(), postgres_version: scalar(databaseUrl, "SHOW server_version;", timeout), dataset: { request_rows: requestRows, terminal_request_rows: terminalRequestRows, request_fact_rows: factRows, terminal_generation_rows: terminalGenerationRows, generation_fact_rows: generationFactRows, blank_currency_rows: blankCurrencyRows, event_rows: eventRows }, thresholds: { max_execution_ms: args.maxExecutionMs, min_request_rows: args.minRequestRows, max_sequential_scan_rows: args.maxSequentialScanRows, allow_sequential_scan: args.allowSequentialScan }, results, checks, passed, exit_code: passed ? 0 : 2 };
    writeFileSync(output, `${JSON.stringify(report, null, 2)}\n`);
    console.log(JSON.stringify({ passed, checks }, null, 2));
    return report.exit_code;
  } catch (error) { return prerequisite(error); }
}

if (import.meta.url === `file://${process.argv[1]}`) process.exitCode = main();
