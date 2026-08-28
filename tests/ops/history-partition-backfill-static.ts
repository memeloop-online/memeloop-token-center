import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { parseArguments, validateUtcDate } from "../../ops/backfill-postgres-history-partitions.ts";

const repository = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const driver = resolve(repository, "ops/backfill-postgres-history-partitions.ts");
const procedure = resolve(repository, "ops/postgres/history-partition-backfill.sql");
const indexes = resolve(repository, "ops/postgres/history-partition-indexes.sql");

function source(path: string): string {
  return readFileSync(path, "utf8");
}

test("operator defaults to a bounded read-only plan", () => {
  assert.deepEqual(parseArguments([], "2026-08-28"), {
    action: "dry-run",
    indexesOnly: false,
    tableSelection: "all",
    beforeDate: "2026-08-28",
    batchSize: 10_000,
    maxDays: 1,
  });
  assert.throws(() => parseArguments(["--indexes-only"], "2026-08-28"), /requires --apply/u);
  assert.throws(() => parseArguments(["--batch-size", "0"], "2026-08-28"), /between 1 and 100000/u);
  assert.throws(() => parseArguments(["--batch-size", "100001"], "2026-08-28"), /between 1 and 100000/u);
  assert.throws(() => parseArguments(["--max-days", "0"], "2026-08-28"), /at least 1/u);
  assert.throws(() => parseArguments(["--from", "2026-08-28"], "2026-08-28"), /earlier than --before/u);
});

test("UTC date parser rejects normalized and impossible dates", () => {
  assert.doesNotThrow(() => validateUtcDate("2024-02-29"));
  for (const value of ["2023-02-29", "2026-2-01", "2026-13-01", "not-a-date"]) {
    assert.throws(() => validateUtcDate(value), /invalid UTC date/u);
  }
});

test("help is available without PostgreSQL credentials", () => {
  const result = spawnSync(process.execPath, [driver, "--help"], { encoding: "utf8", env: {}, shell: false });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /Read-only dry-run is the default/u);
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

test("TypeScript driver and SQL retain low-lock and atomic safety markers", () => {
  const driverSource = source(driver);
  const procedureSource = source(procedure);
  const indexSource = source(indexes);
  for (const marker of ["action: \"dry-run\"", "--apply", "pg_try_advisory_lock", "CREATE INDEX CONCURRENTLY", "shell: false"]) {
    assert.ok(driverSource.includes(marker), `missing required safety marker in ${driver}: ${marker}`);
  }
  for (const marker of ["COMMIT;", "LOCK TABLE public.%I IN SHARE ROW EXCLUSIVE MODE", "ON CONFLICT (%4$I) DO NOTHING", "EXCEPT SELECT * FROM public.%2$I", "ATTACH PARTITION"]) {
    assert.ok(procedureSource.includes(marker), `missing required safety marker in ${procedure}: ${marker}`);
  }
  for (const marker of ["ON ONLY public.request_records (created_at DESC, id DESC)", "ON ONLY public.request_events (event_at ASC, event_id ASC)"]) {
    assert.ok(indexSource.includes(marker), `missing required safety marker in ${indexes}: ${marker}`);
  }
  assert.ok(!`${driverSource}${procedureSource}${indexSource}`.includes("migrations/sqlite"), "PostgreSQL backfill must not reference SQLite migrations");
});
