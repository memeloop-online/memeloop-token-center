#!/usr/bin/env node
/** Read-only PostgreSQL EXPLAIN gate for imported-scale conversation paging. */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

const SAFE_UUID = /^[0-9A-Fa-f-]{36}$/;

export class GateFailure extends Error {}

type JsonObject = Record<string, any>;

interface Arguments {
  databaseUrlFile?: string;
  output?: string;
  minRequestRows: number;
  maxExecutionMs: number;
  statementTimeoutMs: number;
}

function parseNumber(flag: string, value: string | undefined): number {
  if (value === undefined || !Number.isFinite(Number(value))) {
    throw new GateFailure(`${flag} requires a number`);
  }
  return Number(value);
}

function argumentsFrom(argv = process.argv.slice(2)): Arguments {
  const args: Arguments = {
    minRequestRows: 110_000,
    maxExecutionMs: 250,
    statementTimeoutMs: 30_000,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (flag === "--database-url-file") args.databaseUrlFile = value;
    else if (flag === "--output") args.output = value;
    else if (flag === "--min-request-rows") args.minRequestRows = parseNumber(flag, value);
    else if (flag === "--max-execution-ms") args.maxExecutionMs = parseNumber(flag, value);
    else if (flag === "--statement-timeout-ms") args.statementTimeoutMs = parseNumber(flag, value);
    else if (flag === "--help" || flag === "-h") {
      console.log("Usage: conversation_explain.ts [--database-url-file PATH] [--output PATH] [--min-request-rows N] [--max-execution-ms N] [--statement-timeout-ms N]");
      process.exit(0);
    } else throw new GateFailure(`unrecognized argument: ${flag}`);
    index += 1;
  }
  return args;
}

function executableExists(name: string): boolean {
  return (process.env.PATH ?? "").split(":").some((directory) => existsSync(`${directory}/${name}`));
}

export function psql(databaseUrl: string, sql: string, timeoutMs: number): string {
  const result = spawnSync("psql", ["-X", "-q", "-A", "-t", "--no-psqlrc"], {
    input: sql,
    encoding: "utf8",
    timeout: Math.max(30_000, timeoutMs + 10_000),
    env: {
      ...process.env,
      PGDATABASE: databaseUrl,
      PGOPTIONS: `${process.env.PGOPTIONS ?? ""} -c default_transaction_read_only=on -c statement_timeout=${timeoutMs}`.trim(),
    },
  });
  if (result.error) throw new GateFailure(result.error.message);
  if (result.status !== 0) {
    const lines = (result.stderr ?? "").trim().split(/\r?\n/u).filter(Boolean);
    throw new GateFailure(lines.at(-1) ?? "psql failed");
  }
  return (result.stdout ?? "").trim();
}

function scalar(databaseUrl: string, sql: string, timeoutMs: number): string {
  return psql(databaseUrl, sql, timeoutMs).split(/\r?\n/u).at(-1)?.trim() ?? "";
}

function nodes(plan: JsonObject): JsonObject[] {
  return [plan, ...(Array.isArray(plan.Plans) ? plan.Plans.flatMap((child: JsonObject) => nodes(child)) : [])];
}

function explain(databaseUrl: string, name: string, query: string, timeoutMs: number): JsonObject {
  const parsed = JSON.parse(psql(databaseUrl, `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) ${query};`, timeoutMs)) as JsonObject[];
  const document = parsed[0]!;
  const plan = document.Plan as JsonObject;
  const planNodes = nodes(plan);
  const relations = ["request_records", "conversation_key_clusters", "conversation_observations", "conversation_edges"];
  return {
    name,
    execution_time_ms: document["Execution Time"] ?? 0,
    returned_rows: plan["Actual Rows"] ?? 0,
    indexes: [...new Set(planNodes.map((node) => node["Index Name"]).filter((value): value is string => value !== undefined))].sort(),
    forbidden_sequential_scans: [...new Set(planNodes.filter((node) => node["Node Type"] === "Seq Scan" && relations.some((relation) => String(node["Relation Name"] ?? "").startsWith(relation))).map((node) => String(node["Relation Name"])))].sort(),
    plan,
  };
}

export function main(argv = process.argv.slice(2)): number {
  let args: Arguments;
  try {
    args = argumentsFrom(argv);
  } catch (error) {
    console.error(JSON.stringify({ passed: false, error: error instanceof Error ? error.message : String(error) }));
    return 3;
  }
  if (!executableExists("psql")) {
    console.error(JSON.stringify({ passed: false, error: "psql is not installed" }));
    return 3;
  }
  try {
    const databaseUrl = (args.databaseUrlFile ? readFileSync(args.databaseUrlFile, "utf8") : process.env.MTC_BENCH_DATABASE_URL ?? "").trim();
    if (!databaseUrl) throw new GateFailure("MTC_BENCH_DATABASE_URL or --database-url-file is required");
    const requestRows = Number.parseInt(scalar(databaseUrl, "SELECT COUNT(*) FROM request_records;", args.statementTimeoutMs), 10);
    const projectionRows = Number.parseInt(scalar(databaseUrl, "SELECT COUNT(*) FROM conversation_key_clusters;", args.statementTimeoutMs), 10);
    const sample = scalar(databaseUrl, "SELECT key_id || '|' || cluster_id FROM conversation_key_clusters ORDER BY request_count DESC, updated_at DESC LIMIT 1;", args.statementTimeoutMs).split("|");
    if (sample.length !== 2 || !sample.every((value) => SAFE_UUID.test(value))) throw new GateFailure("no valid conversation projection is available");
    const [keyId, clusterId] = sample as [string, string];
    let cursor = scalar(databaseUrl, `SELECT created_at::text || '|' || id FROM request_records WHERE key_id = '${keyId}' AND conversation_cluster_id = '${clusterId}' ORDER BY created_at DESC, id DESC OFFSET 99 LIMIT 1;`, args.statementTimeoutMs).split("|");
    if (cursor.length !== 2 || !/^\d+$/u.test(cursor[0] ?? "") || !SAFE_UUID.test(cursor[1] ?? "")) cursor = ["9223372036854775807", "ffffffff-ffff-ffff-ffff-ffffffffffff"];
    const [beforeCreatedAt, beforeRequestId] = cursor as [string, string];
    const queries: Array<[string, string]> = [
      ["conversation_list_first_page", `SELECT p.cluster_id, c.explicit_session_id, p.updated_at, p.request_count, p.candidate_edge_count FROM conversation_key_clusters p JOIN conversation_clusters c ON c.id = p.cluster_id WHERE p.key_id = '${keyId}' ORDER BY p.updated_at DESC, p.cluster_id DESC LIMIT 50`],
      ["conversation_detail_membership", `SELECT p.cluster_id, p.request_count FROM conversation_key_clusters p WHERE p.key_id = '${keyId}' AND p.cluster_id = '${clusterId}'`],
      ["conversation_detail_first_page", `SELECT id, created_at, protocol, model, status_code FROM request_records WHERE key_id = '${keyId}' AND conversation_cluster_id = '${clusterId}' ORDER BY created_at DESC, id DESC LIMIT 201`],
      ["conversation_detail_older_page", `SELECT id, created_at, protocol, model, status_code FROM request_records WHERE key_id = '${keyId}' AND conversation_cluster_id = '${clusterId}' AND (created_at < ${beforeCreatedAt} OR (created_at = ${beforeCreatedAt} AND id < '${beforeRequestId}')) ORDER BY created_at DESC, id DESC LIMIT 201`],
    ];
    const results = queries.map(([name, query]) => explain(databaseUrl, name, query, args.statementTimeoutMs));
    const checks: JsonObject[] = [
      { name: "imported request scale", actual: requestRows, expected_minimum: args.minRequestRows, passed: requestRows >= args.minRequestRows },
      { name: "conversation projections populated", actual: projectionRows, expected_minimum: 1, passed: projectionRows > 0 },
    ];
    for (const result of results) checks.push(
      { name: `${result.name} latency`, actual: result.execution_time_ms, expected_maximum: args.maxExecutionMs, passed: result.execution_time_ms <= args.maxExecutionMs },
      { name: `${result.name} indexed`, actual: result.forbidden_sequential_scans, expected: [], passed: result.forbidden_sequential_scans.length === 0 },
    );
    const report = { schema_version: 1, dataset: { request_rows: requestRows, projection_rows: projectionRows }, sample: { key_id: keyId, cluster_id: clusterId }, thresholds: { min_request_rows: args.minRequestRows, max_execution_ms: args.maxExecutionMs }, results, checks, passed: checks.every((check) => check.passed) };
    if (args.output) { mkdirSync(dirname(args.output), { recursive: true }); writeFileSync(args.output, `${JSON.stringify(report, null, 2)}\n`); }
    console.log(JSON.stringify({ passed: report.passed, checks }, null, 2));
    return report.passed ? 0 : 2;
  } catch (error) {
    console.error(JSON.stringify({ passed: false, error: error instanceof Error ? error.message : String(error) }));
    return 3;
  }
}

if (import.meta.url === `file://${process.argv[1]}`) process.exitCode = main();
