#!/usr/bin/env node
/** Safely inventory, rebuild, or prune PostgreSQL observability projections. */

import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const usage = `Usage: reconcile-postgres-request-stats.ts [options]

Read-only inventory is the default. PostgreSQL libpq variables PGHOST, PGUSER,
PGPASSWORD and PGDATABASE are required; PGPORT defaults to 5432.

Options:
  --apply                 Rebuild selected completed UTC days.
  --from YYYY-MM-DD       Include days on/after this date.
  --before YYYY-MM-DD     Exclude days on/after this date (default today UTC).
  --max-days N            Maximum days rebuilt (default 1).
  --prune-before DATE     Inventory or prune stats strictly before a UTC date.
  --confirm-prune         Required with --apply --prune-before.
  --help                  Show this help.

The prune mode refuses to run while request_records or generation_jobs still
contains a row before the cutoff. Archive retention must be verified separately
before pruning facts and their hourly/daily projections.
`;

type Action = "dry-run" | "apply";
interface Options {
  action: Action;
  fromDate?: string;
  beforeDate: string;
  maxDays: number;
  pruneBefore?: string;
  confirmPrune: boolean;
}

class CliFailure extends Error {
  readonly exitCode: number;
  constructor(message: string, exitCode = 2) {
    super(message);
    this.exitCode = exitCode;
  }
}

class PsqlFailure extends Error {
  readonly exitCode: number;
  constructor(exitCode: number) {
    super(`psql exited with status ${exitCode}`);
    this.exitCode = exitCode;
  }
}

function utcToday(): string {
  return new Date().toISOString().slice(0, 10);
}

function takeValue(argv: string[], index: number, flag: string): string {
  const value = argv[index + 1];
  if (value === undefined) throw new CliFailure(`${flag} requires a value`);
  return value;
}

function positiveInteger(flag: string, raw: string): number {
  if (!/^[0-9]+$/u.test(raw)) throw new CliFailure(`${flag} must be an integer`);
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < 1) throw new CliFailure(`${flag} must be at least 1`);
  return value;
}

export function validateUtcDate(value: string): void {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/u.exec(value);
  if (match === null) throw new CliFailure(`invalid UTC date: ${value}`);
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const candidate = new Date(Date.UTC(year, month - 1, day));
  if (candidate.getUTCFullYear() !== year || candidate.getUTCMonth() !== month - 1 || candidate.getUTCDate() !== day) {
    throw new CliFailure(`invalid UTC date: ${value}`);
  }
}

export function parseArguments(argv: string[], today = utcToday()): Options | undefined {
  const options: Options = {
    action: "dry-run",
    beforeDate: today,
    maxDays: 1,
    confirmPrune: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--help" || flag === "-h") return undefined;
    if (flag === "--apply") options.action = "apply";
    else if (flag === "--confirm-prune") options.confirmPrune = true;
    else if (flag === "--from") {
      options.fromDate = takeValue(argv, index, flag);
      index += 1;
    } else if (flag === "--before") {
      options.beforeDate = takeValue(argv, index, flag);
      index += 1;
    } else if (flag === "--max-days") {
      options.maxDays = positiveInteger(flag, takeValue(argv, index, flag));
      index += 1;
    } else if (flag === "--prune-before") {
      options.pruneBefore = takeValue(argv, index, flag);
      index += 1;
    } else {
      throw new CliFailure(`unknown option: ${flag}`);
    }
  }
  validateUtcDate(options.beforeDate);
  if (options.fromDate !== undefined) validateUtcDate(options.fromDate);
  if (options.pruneBefore !== undefined) validateUtcDate(options.pruneBefore);
  if (options.pruneBefore !== undefined && options.fromDate !== undefined) {
    throw new CliFailure("--prune-before cannot be combined with --from");
  }
  if (options.confirmPrune && (options.action !== "apply" || options.pruneBefore === undefined)) {
    throw new CliFailure("--confirm-prune requires --apply --prune-before");
  }
  if (options.action === "apply" && options.pruneBefore !== undefined && !options.confirmPrune) {
    throw new CliFailure("--apply --prune-before requires --confirm-prune");
  }
  return options;
}

function requiredLibpqEnvironment(): NodeJS.ProcessEnv {
  const environment = { ...process.env };
  for (const name of ["PGHOST", "PGUSER", "PGPASSWORD", "PGDATABASE"] as const) {
    if (!environment[name]) throw new CliFailure(`${name} is required`);
  }
  environment.PGPORT ||= "5432";
  return environment;
}

interface PsqlInvocation {
  args?: string[];
  input?: string;
  capture?: boolean;
}

function psql(environment: NodeJS.ProcessEnv, invocation: PsqlInvocation = {}): string {
  const result = spawnSync(
    "psql",
    ["-X", "-v", "ON_ERROR_STOP=1", "--no-psqlrc", ...(invocation.args ?? [])],
    { encoding: "utf8", env: environment, input: invocation.input, shell: false },
  );
  if (result.error !== undefined) {
    const exitCode = (result.error as NodeJS.ErrnoException).code === "ENOENT" ? 127 : 1;
    throw new CliFailure(`unable to run psql: ${result.error.message}`, exitCode);
  }
  if (result.stderr) process.stderr.write(result.stderr);
  if (result.status !== 0) throw new PsqlFailure(result.status ?? 1);
  const output = result.stdout ?? "";
  if (!invocation.capture && output) process.stdout.write(output);
  return output.trim();
}

function variables(values: Record<string, string | number | undefined>): string[] {
  return Object.entries(values).flatMap(([name, value]) => ["-v", `${name}=${value ?? ""}`]);
}

function pruneInventory(environment: NodeJS.ProcessEnv, cutoff: string): void {
  psql(environment, { args: variables({ cutoff }), input: `WITH boundary AS (
  SELECT (extract(epoch FROM (:'cutoff'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AS millis
)
SELECT :'cutoff' AS prune_before_utc,
       (SELECT count(*) FROM request_records, boundary WHERE created_at < boundary.millis) AS retained_raw_rows,
       (SELECT count(*) FROM generation_jobs, boundary WHERE created_at < boundary.millis) AS retained_generation_rows,
       (SELECT count(*) FROM request_stats_facts, boundary WHERE created_at < boundary.millis) AS fact_rows,
       (SELECT count(*) FROM generation_stats_facts, boundary WHERE created_at < boundary.millis) AS generation_fact_rows,
       (SELECT COALESCE(sum(requests), 0) FROM request_daily_aggregates
         WHERE day_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint) AS aggregate_requests,
       (SELECT COALESCE(sum(requests), 0) FROM generation_daily_aggregates
         WHERE day_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint) AS generation_aggregate_requests,
       (SELECT COALESCE(sum(requests), 0) FROM usage_analysis_hourly
         WHERE hour_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint * 24) AS analysis_hourly_requests,
       (SELECT COALESCE(sum(requests), 0) FROM usage_analysis_daily
         WHERE day_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint) AS analysis_daily_requests;
` });
}

function pruneApply(environment: NodeJS.ProcessEnv, cutoff: string): void {
  psql(environment, { args: variables({ cutoff }), input: `BEGIN;
SELECT pg_advisory_xact_lock(hashtextextended('memeloop-token-center:request-stats', 734627102948314));
SET LOCAL lock_timeout = '5s';
LOCK TABLE request_records, generation_jobs IN SHARE MODE;
CREATE TEMP TABLE mtc_request_stats_prune_guard (
  invalid boolean NOT NULL CHECK (invalid = false)
) ON COMMIT DROP;
INSERT INTO mtc_request_stats_prune_guard (invalid)
SELECT true
 WHERE EXISTS (
   SELECT 1 FROM request_records
    WHERE created_at < (extract(epoch FROM (:'cutoff'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
   UNION ALL
   SELECT 1 FROM generation_jobs
    WHERE created_at < (extract(epoch FROM (:'cutoff'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
 );
DELETE FROM usage_analysis_daily
 WHERE day_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint;
DELETE FROM usage_analysis_hourly
 WHERE hour_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint * 24;
DELETE FROM request_daily_aggregates
 WHERE day_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint;
DELETE FROM generation_daily_aggregates
 WHERE day_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint;
DELETE FROM request_stats_facts
 WHERE created_at < (extract(epoch FROM (:'cutoff'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint;
DELETE FROM generation_stats_facts
 WHERE created_at < (extract(epoch FROM (:'cutoff'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint;
COMMIT;
` });
}

function selectedDays(environment: NodeJS.ProcessEnv, options: Options): string[] {
  const fromPredicate = options.fromDate === undefined
    ? ""
    : "created_at >= (extract(epoch FROM (:'from_date'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AND";
  const output = psql(environment, {
    args: ["-At", ...variables({ from_date: options.fromDate, before_date: options.beforeDate })],
    capture: true,
    input: `SELECT day
  FROM (
    SELECT to_char(to_timestamp(created_at / 1000.0) AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS day
      FROM request_records
     WHERE ${fromPredicate}
           created_at < (extract(epoch FROM (:'before_date'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
       AND completed_at IS NOT NULL AND status_code IS NOT NULL
    UNION
    SELECT to_char(to_timestamp(created_at / 1000.0) AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS day
      FROM generation_stats_facts
     WHERE ${fromPredicate}
           created_at < (extract(epoch FROM (:'before_date'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
  ) completed_days
 ORDER BY 1
 LIMIT ${options.maxDays};
`,
  });
  return output.split(/\r?\n/u).filter(Boolean);
}

function inventoryDay(environment: NodeJS.ProcessEnv, day: string): void {
  psql(environment, { args: variables({ day }), input: `WITH bounds AS (
  SELECT (extract(epoch FROM (:'day'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AS start_ms,
         (extract(epoch FROM ((:'day'::date + 1)::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AS end_ms
)
SELECT (SELECT count(*) FROM request_records, bounds
         WHERE created_at >= start_ms AND created_at < end_ms
           AND completed_at IS NOT NULL AND status_code IS NOT NULL) AS terminal_raw_rows,
       (SELECT count(*) FROM request_stats_facts, bounds
         WHERE created_at >= start_ms AND created_at < end_ms) AS fact_rows,
       (SELECT count(*) FROM generation_stats_facts, bounds
         WHERE created_at >= start_ms AND created_at < end_ms) AS generation_fact_rows,
       (SELECT COALESCE(sum(requests), 0) FROM request_daily_aggregates
         WHERE day_bucket = (:'day'::date - DATE '1970-01-01')::bigint) AS aggregate_requests,
       (SELECT COALESCE(sum(requests), 0) FROM generation_daily_aggregates
         WHERE day_bucket = (:'day'::date - DATE '1970-01-01')::bigint) AS generation_aggregate_requests,
       (SELECT COALESCE(sum(requests), 0) FROM usage_analysis_hourly
         WHERE hour_bucket >= (:'day'::date - DATE '1970-01-01')::bigint * 24
           AND hour_bucket < ((:'day'::date - DATE '1970-01-01')::bigint + 1) * 24) AS analysis_hourly_requests,
       (SELECT COALESCE(sum(requests), 0) FROM usage_analysis_daily
         WHERE day_bucket = (:'day'::date - DATE '1970-01-01')::bigint) AS analysis_daily_requests;
` });
}

export function main(argv = process.argv.slice(2)): number {
  try {
    const options = parseArguments(argv);
    if (options === undefined) {
      process.stdout.write(usage);
      return 0;
    }
    const environment = requiredLibpqEnvironment();
    if (options.pruneBefore !== undefined) {
      pruneInventory(environment, options.pruneBefore);
      if (options.action !== "apply") {
        console.log("DRY RUN: no request statistics were pruned.");
        return 0;
      }
      pruneApply(environment, options.pruneBefore);
      console.log(`Pruned request statistics strictly before ${options.pruneBefore} after verifying raw history was absent.`);
      return 0;
    }
    const days = selectedDays(environment, options);
    if (days.length === 0) {
      console.log("No completed UTC request days matched the selected range.");
      return 0;
    }
    const reconcileSql = resolve(dirname(fileURLToPath(import.meta.url)), "postgres/reconcile-observability-day.sql");
    for (const day of days) {
      console.log(`Request statistics UTC day ${day}:`);
      inventoryDay(environment, day);
      if (options.action === "apply") psql(environment, { args: [...variables({ day }), "-f", reconcileSql] });
    }
    if (options.action === "dry-run") {
      console.log(`DRY RUN: no request statistics were changed. Re-run with --apply to rebuild at most ${options.maxDays} day(s).`);
    }
    return 0;
  } catch (error) {
    if (error instanceof CliFailure || error instanceof PsqlFailure) {
      console.error(error.message);
      if (error instanceof CliFailure && error.exitCode === 2) process.stderr.write(usage);
      return error.exitCode;
    }
    console.error(error instanceof Error ? error.message : String(error));
    return 1;
  }
}

if (import.meta.url === `file://${process.argv[1]}`) process.exitCode = main();
