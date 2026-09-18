import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { parseArguments, validateUtcDate } from "../../scripts/maintenance/reconcile-postgres-request-stats.ts";

const repository = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const driver = resolve(repository, "scripts/maintenance/reconcile-postgres-request-stats.ts");
const daySql = resolve(repository, "scripts/maintenance/reconcile-observability-day.sql");

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
    "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
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
  const sourceTableLock = pruneSource.indexOf("LOCK TABLE request_records, generation_jobs IN SHARE MODE");
  assert.notEqual(exclusiveProjectionLock, -1, "prune must acquire the exclusive projection lock");
  assert.notEqual(sourceTableLock, -1, "prune must lock both source tables against terminal writers");
  assert.ok(
    exclusiveProjectionLock < sourceTableLock,
    "the exclusive projection lock must precede source-table locks to match online writer ordering",
  );
});
