import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { DatabaseSync } from "node:sqlite";

import { parseArguments, validateUtcDate } from "../../scripts/maintenance/reconcile-postgres-request-stats.ts";

const repository = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const driver = resolve(repository, "scripts/maintenance/reconcile-postgres-request-stats.ts");
const daySql = resolve(repository, "scripts/maintenance/reconcile-observability-day.sql");

test("retained-record reconciliation preserves latest corrections and uncorrected costs on replay", () => {
  const sqlSource = readFileSync(daySql, "utf8");
  const retainedInsert = sqlSource.slice(sqlSource.indexOf("INSERT INTO request_stats_facts", sqlSource.indexOf("INSERT INTO request_stats_facts") + 1));
  const statement = retainedInsert.slice(0, retainedInsert.indexOf(";"));
  const columns = statement.slice(statement.indexOf("(") + 1, statement.indexOf(")")).split(",").map((column) => column.trim());
  const database = new DatabaseSync(":memory:");
  try {
    database.exec(`
      CREATE TABLE request_stats_facts (${columns.map((column) => `${column} ${column === "request_id" ? "TEXT PRIMARY KEY" : ""}`).join(",")});
      CREATE TABLE request_records (id TEXT, ${columns.filter((column) => column !== "request_id" && column !== "status_class").join(",")}, completed_at, status_code, usage_basis TEXT);
      CREATE TABLE key_records (id TEXT, tenant_id TEXT, currency TEXT);
      CREATE TABLE terminal_projection_outbox (request_id TEXT);
      CREATE TABLE mtc_reconcile_day_bounds (start_ms, end_ms);
      CREATE TABLE request_cost_projection_corrections (request_id TEXT, corrected_fact_cost_micros, applied_at, correction_version TEXT);
      INSERT INTO key_records VALUES ('key', 'tenant', 'USD');
      INSERT INTO mtc_reconcile_day_bounds VALUES (0, 100);
    `);
    for (const [request, status, cost, basis] of [["corrected", 499, 900, "contract_ceiling"], ["provider", 499, 40, "provider_reported"], ["legacy-null", 504, 50, null], ["success", 200, 60, "provider_reported"]] as const) {
      database.prepare("INSERT INTO request_records (id, tenant_id, key_id, created_at, completed_at, status_code, cost_micros, currency, usage_basis) VALUES (?, 'tenant', 'key', 1, 2, ?, ?, 'USD', ?)").run(request, status, cost, basis);
    }
    database.exec(`
      INSERT INTO request_cost_projection_corrections VALUES ('corrected', 30, 1, 'v1'), ('corrected', 20, 2, 'v1'), ('corrected', 0, 2, 'v2');
      INSERT INTO request_stats_facts (request_id, cost_micros) VALUES ('corrected', 999);
    `);
    const expected = [["corrected", 0], ["legacy-null", 50], ["provider", 40], ["success", 60]];
    for (const replay of [1, 2]) {
      database.exec(statement);
      assert.deepEqual(database.prepare("SELECT request_id, cost_micros FROM request_stats_facts ORDER BY request_id").all().map((row) => [row.request_id, row.cost_micros]), expected, `replay ${replay}`);
      assert.equal(database.prepare("SELECT cost_micros FROM request_records WHERE id = 'corrected'").get()?.cost_micros, 900);
    }
  } finally {
    database.close();
  }
});

test("reconciliation defaults to a bounded read-only inventory", () => {
  assert.deepEqual(parseArguments([], "2026-08-28"), {
    action: "dry-run",
    beforeDate: "2026-08-28",
    maxDays: 1,
    confirmPrune: false,
  });
  assert.throws(() => parseArguments(["--max-days", "0"], "2026-08-28"), /at least 1/u);
  assert.throws(() => parseArguments(["--prune-before", "2026-01-01", "--from", "2025-01-01"], "2026-08-28"), /cannot be combined/u);
});

test("pruning requires both explicit mutation flags", () => {
  assert.throws(() => parseArguments(["--apply", "--prune-before", "2026-01-01"], "2026-08-28"), /requires --confirm-prune/u);
  assert.throws(() => parseArguments(["--confirm-prune"], "2026-08-28"), /requires --apply --prune-before/u);
  const parsed = parseArguments(["--apply", "--prune-before", "2026-01-01", "--confirm-prune"], "2026-08-28");
  assert.equal(parsed?.action, "apply");
  assert.equal(parsed?.pruneBefore, "2026-01-01");
  assert.equal(parsed?.confirmPrune, true);
});

test("UTC date parser accepts leap days and rejects impossible dates", () => {
  assert.doesNotThrow(() => validateUtcDate("2024-02-29"));
  assert.throws(() => validateUtcDate("2026-02-29"), /invalid UTC date/u);
});

test("help is available without PostgreSQL credentials", () => {
  const result = spawnSync(process.execPath, [driver, "--help"], { encoding: "utf8", env: {}, shell: false });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Read-only inventory is the default/u);
});

test("libpq credentials stay out of argv and diagnostics", () => {
  const secret = "must-not-appear-in-process-output";
  const result = spawnSync(process.execPath, [driver], {
    encoding: "utf8",
    env: { PATH: "", PGHOST: "database.internal", PGUSER: "operator", PGPASSWORD: secret, PGDATABASE: "token_center" },
    shell: false,
  });
  assert.equal(result.status, 127, result.stderr);
  assert.ok(!`${result.stdout}${result.stderr}`.includes(secret));
});

test("driver and day rebuild SQL retain reconciliation safety markers", () => {
  const driverSource = readFileSync(driver, "utf8");
  const sqlSource = readFileSync(daySql, "utf8");
  for (const marker of [
    "action: \"dry-run\"",
    "--confirm-prune",
    "pg_advisory_xact_lock",
    "mtc_request_stats_prune_guard",
    "DELETE FROM request_daily_aggregates",
    "DELETE FROM generation_daily_aggregates",
    "DELETE FROM usage_analysis_hourly",
    "DELETE FROM usage_analysis_daily",
    "DELETE FROM generation_stats_facts",
    "DRY RUN: no request statistics were changed",
    "shell: false",
  ]) assert.ok(driverSource.includes(marker), `missing request-stats safety marker: ${marker}`);
  for (const marker of [
    "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
    "IN SHARE ROW EXCLUSIVE MODE",
    "ON CONFLICT (request_id) DO UPDATE SET",
    "cached_input_tokens, cache_write_tokens",
    "service_tier, currency",
    "'request'",
    "'generation'",
    "INSERT INTO usage_analysis_hourly",
    "INSERT INTO usage_analysis_daily",
    "FROM usage_analysis_hourly h",
  ]) assert.ok(sqlSource.includes(marker), `missing observability rebuild marker: ${marker}`);

  const pruneSource = driverSource.slice(driverSource.indexOf("function pruneApply"));
  const exclusiveProjectionLock = pruneSource.indexOf("SELECT pg_advisory_xact_lock");
  const pendingWorkGuard = pruneSource.indexOf("SELECT 1 FROM terminal_projection_outbox");
  assert.notEqual(exclusiveProjectionLock, -1, "prune must acquire the exclusive projection lock");
  assert.notEqual(pendingWorkGuard, -1, "prune must reject pending durable terminal work");
  assert.ok(
    exclusiveProjectionLock < pendingWorkGuard,
    "the exclusive projection lock must precede the durable-work guard",
  );
  assert.ok(!pruneSource.includes("LOCK TABLE request_records"));
  assert.ok(pruneSource.includes("INSERT INTO observability_prune_boundaries"));
  assert.ok(sqlSource.includes("FROM observability_prune_boundaries"));
  assert.ok(sqlSource.includes("t.statistics_outcome = 'applied'"));
});
