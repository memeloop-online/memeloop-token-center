#!/usr/bin/env node
/** Safely inventory or repair PostgreSQL history default partitions. */

import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const usage = `Usage: backfill-postgres-history-partitions.ts [options]

Read-only dry-run is the default. PostgreSQL libpq variables PGHOST, PGUSER,
PGPASSWORD and PGDATABASE are required; PGPORT defaults to 5432.

Options:
  --apply                 Install operational SQL/indexes and move data.
  --indexes-only          With --apply, install/verify indexes but move no rows.
  --table NAME            request_records, request_events, or all (default all).
  --from YYYY-MM-DD       Include completed UTC days on/after this date.
  --before YYYY-MM-DD     Exclude days on/after this date (default today UTC).
  --batch-size N          Copy transaction size, 1..100000 (default 10000).
  --max-days N            Maximum days processed per table (default 1).
  --help                  Show this help.

Examples:
  # Safe inventory only:
  ./ops/backfill-postgres-history-partitions.ts

  # Move one oldest completed request day, in 5,000-row copy transactions:
  ./ops/backfill-postgres-history-partitions.ts --apply \\
    --table request_records --batch-size 5000 --max-days 1

Repeat the apply command until the dry-run reports zero default-partition rows.
`;

type Action = "dry-run" | "apply";
type TableName = "request_records" | "request_events";
interface Options {
  action: Action;
  indexesOnly: boolean;
  tableSelection: TableName | "all";
  fromDate?: string;
  beforeDate: string;
  batchSize: number;
  maxDays: number;
}

class CliFailure extends Error {
  readonly exitCode: number;
  constructor(message: string, exitCode = 2) {
    super(message);
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

function positiveInteger(flag: string, raw: string, maximum?: number): number {
  if (!/^[0-9]+$/u.test(raw)) throw new CliFailure(`${flag} must be an integer`);
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < 1) {
    throw new CliFailure(maximum === undefined ? `${flag} must be at least 1` : `${flag} must be between 1 and ${maximum}`);
  }
  if (maximum !== undefined && value > maximum) throw new CliFailure(`${flag} must be between 1 and ${maximum}`);
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
    indexesOnly: false,
    tableSelection: "all",
    beforeDate: today,
    batchSize: 10_000,
    maxDays: 1,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--help" || flag === "-h") return undefined;
    if (flag === "--apply") options.action = "apply";
    else if (flag === "--indexes-only") options.indexesOnly = true;
    else if (flag === "--table") {
      const value = takeValue(argv, index, flag);
      if (value !== "request_records" && value !== "request_events" && value !== "all") {
        throw new CliFailure("--table must be request_records, request_events, or all");
      }
      options.tableSelection = value;
      index += 1;
    } else if (flag === "--from") {
      options.fromDate = takeValue(argv, index, flag);
      index += 1;
    } else if (flag === "--before") {
      options.beforeDate = takeValue(argv, index, flag);
      index += 1;
    } else if (flag === "--batch-size") {
      options.batchSize = positiveInteger(flag, takeValue(argv, index, flag), 100_000);
      index += 1;
    } else if (flag === "--max-days") {
      options.maxDays = positiveInteger(flag, takeValue(argv, index, flag));
      index += 1;
    } else {
      throw new CliFailure(`unknown option: ${flag}`);
    }
  }
  validateUtcDate(options.beforeDate);
  if (options.fromDate !== undefined) {
    validateUtcDate(options.fromDate);
    if (options.fromDate >= options.beforeDate) throw new CliFailure("--from must be earlier than --before");
  }
  if (options.indexesOnly && options.action !== "apply") throw new CliFailure("--indexes-only requires --apply");
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

class PsqlFailure extends Error {
  readonly exitCode: number;
  constructor(exitCode: number) {
    super(`psql exited with status ${exitCode}`);
    this.exitCode = exitCode;
  }
}

function psql(environment: NodeJS.ProcessEnv, invocation: PsqlInvocation = {}): string {
  const result = spawnSync(
    "psql",
    ["-X", "--no-psqlrc", "-v", "ON_ERROR_STOP=1", ...(invocation.args ?? [])],
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

function tableNames(selection: Options["tableSelection"]): TableName[] {
  return selection === "all" ? ["request_records", "request_events"] : [selection];
}

function timeColumn(table: TableName): "created_at" | "event_at" {
  return table === "request_records" ? "created_at" : "event_at";
}

function datePredicate(column: string, fromDate: string | undefined): string {
  const lower = fromDate === undefined ? "" : `${column} >= ((:'from_date'::date::timestamp AT TIME ZONE 'UTC')::timestamptz) AND `;
  return `${lower}${column} < ((:'before_date'::date::timestamp AT TIME ZONE 'UTC')::timestamptz)`;
}

function inventorySql(table: TableName, options: Options): string {
  const column = timeColumn(table);
  const defaultTable = `${table}_default`;
  const predicate = datePredicate(`to_timestamp(${column} / 1000.0)`, options.fromDate);
  return `SELECT to_char(to_timestamp(${column} / 1000.0) AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS utc_day,
       count(*) AS rows,
       min(${column}) AS minimum_millis,
       max(${column}) AS maximum_millis
  FROM public.${defaultTable}
 WHERE ${predicate}
 GROUP BY 1
 ORDER BY 1
 LIMIT ${options.maxDays};

SELECT count(*) AS selected_rows,
       pg_size_pretty(pg_total_relation_size('public.${defaultTable}')) AS current_default_size
  FROM public.${defaultTable}
 WHERE ${predicate};
`;
}

function planTable(environment: NodeJS.ProcessEnv, table: TableName, options: Options): void {
  console.log(`Plan for public.${table}_default (completed UTC days only):`);
  psql(environment, {
    args: variables({ from_date: options.fromDate, before_date: options.beforeDate }),
    input: inventorySql(table, options),
  });
  psql(environment, {
    args: variables({ table_name: table }),
    input: `SELECT table_name, day_start, status, source_rows, staged_rows, moved_rows,
       batch_size, updated_at, completed_at
  FROM public.mtc_history_partition_backfill_state
 WHERE to_regclass('public.mtc_history_partition_backfill_state') IS NOT NULL
   AND table_name = :'table_name'
 ORDER BY day_start;
`,
  });
}

function stateTableExists(environment: NodeJS.ProcessEnv): boolean {
  return psql(environment, { args: ["-Atc", "SELECT to_regclass('public.mtc_history_partition_backfill_state') IS NOT NULL"], capture: true }) === "t";
}

function dryRun(environment: NodeJS.ProcessEnv, options: Options): void {
  console.log("DRY RUN: no schema or data changes will be made.");
  for (const table of tableNames(options.tableSelection)) {
    if (stateTableExists(environment)) {
      planTable(environment, table, options);
    } else {
      console.log("Operational state table is not installed yet; showing source inventory only.");
      console.log(`Plan for public.${table}_default (completed UTC days only):`);
      psql(environment, {
        args: variables({ from_date: options.fromDate, before_date: options.beforeDate }),
        input: inventorySql(table, options),
      });
    }
  }
  psql(environment, { input: `SELECT index_name, installed, valid
  FROM (
    VALUES
      ('request_records_recent_idx'),
      ('request_records_tenant_time_idx'),
      ('request_records_key_time_idx'),
      ('request_events_global_cursor_idx'),
      ('request_events_tenant_cursor_idx')
  ) required(index_name)
  CROSS JOIN LATERAL (
    SELECT to_regclass('public.' || required.index_name) IS NOT NULL AS installed,
           COALESCE((
             SELECT indisvalid FROM pg_index
              WHERE indexrelid = to_regclass('public.' || required.index_name)
           ), false) AS valid
  ) status
 ORDER BY index_name;
` });
  console.log(`Re-run with --apply to install indexes and process at most ${options.maxDays} day(s) per selected table.`);
}

function captured(environment: NodeJS.ProcessEnv, values: Record<string, string>, sql: string): string {
  return psql(environment, { args: ["-At", ...variables(values)], input: sql, capture: true });
}

function indexLeafName(environment: NodeJS.ProcessEnv, leaf: string, kind: string): string {
  return captured(environment, { leaf, kind }, "SELECT format('mtc_%s_%s_%s', left(:'leaf', 28), :'kind', substr(md5(:'leaf'), 1, 8));\n");
}

function indexIsAttached(environment: NodeJS.ProcessEnv, parentIndex: string, leaf: string): boolean {
  return captured(environment, { parent_index: parentIndex, leaf }, `SELECT EXISTS (
    SELECT 1
      FROM pg_inherits inheritance
      JOIN pg_index child_index ON child_index.indexrelid = inheritance.inhrelid
     WHERE inheritance.inhparent = to_regclass('public.' || :'parent_index')
       AND child_index.indrelid = to_regclass('public.' || :'leaf')
);
`) === "t";
}

function ensureLeafIndex(environment: NodeJS.ProcessEnv, parentTable: TableName, parentIndex: string, kind: string, definition: string): void {
  const leaves = captured(environment, { parent_table: parentTable }, `SELECT child.relname
  FROM pg_inherits inheritance
  JOIN pg_class parent ON parent.oid = inheritance.inhparent
  JOIN pg_namespace parent_namespace ON parent_namespace.oid = parent.relnamespace
  JOIN pg_class child ON child.oid = inheritance.inhrelid
  JOIN pg_namespace child_namespace ON child_namespace.oid = child.relnamespace
 WHERE parent_namespace.nspname = 'public'
   AND child_namespace.nspname = 'public'
   AND parent.relname = :'parent_table'
 ORDER BY child.relname;
`).split(/\r?\n/u).filter(Boolean);

  for (const leaf of leaves) {
    if (indexIsAttached(environment, parentIndex, leaf)) continue;
    const leafIndex = indexLeafName(environment, leaf, kind);
    let indexStatus = captured(environment, { leaf_index: leafIndex }, `SELECT CASE
         WHEN candidate.indexrelid IS NULL THEN 'missing'
         WHEN indisvalid AND indisready THEN 'valid'
         ELSE 'invalid'
       END
  FROM (SELECT to_regclass('public.' || :'leaf_index') AS indexrelid) candidate
  LEFT JOIN pg_index ON pg_index.indexrelid = candidate.indexrelid;
`);
    if (indexStatus === "invalid") {
      console.log(`Dropping invalid interrupted index public.${leafIndex}`);
      psql(environment, { args: variables({ leaf_index: leafIndex }), input: 'DROP INDEX CONCURRENTLY IF EXISTS public.:"leaf_index";\n' });
      indexStatus = "missing";
    }
    if (indexStatus === "missing") {
      console.log(`Building public.${leafIndex} concurrently on public.${leaf}`);
      psql(environment, {
        args: variables({ leaf: leaf, leaf_index: leafIndex }),
        input: `CREATE INDEX CONCURRENTLY :"leaf_index" ON public.:"leaf" (${definition});\n`,
      });
    }
    console.log(`Attaching public.${leafIndex} to public.${parentIndex}`);
    psql(environment, {
      args: variables({ parent_index: parentIndex, leaf_index: leafIndex }),
      input: 'ALTER INDEX public.:"parent_index" ATTACH PARTITION public.:"leaf_index";\n',
    });
  }

  const valid = captured(environment, { parent_index: parentIndex }, `SELECT indisvalid AND indisready
  FROM pg_index
 WHERE indexrelid = to_regclass('public.' || :'parent_index');
`);
  if (valid !== "t") throw new CliFailure(`partitioned index public.${parentIndex} remains invalid; a partition may have appeared concurrently, rerun --apply --indexes-only`, 1);
}

function installIndexes(environment: NodeJS.ProcessEnv, postgresDirectory: string): void {
  console.log("Installing partitioned-index metadata. Leaf builds use CREATE INDEX CONCURRENTLY.");
  psql(environment, { args: ["-f", resolve(postgresDirectory, "history-partition-indexes.sql")] });
  ensureLeafIndex(environment, "request_records", "request_records_recent_idx", "recent", "created_at DESC, id DESC");
  ensureLeafIndex(environment, "request_records", "request_records_tenant_time_idx", "tenant_time", "tenant_id, created_at DESC, id DESC");
  ensureLeafIndex(environment, "request_records", "request_records_key_time_idx", "key_time", "key_id, created_at DESC, id DESC");
  ensureLeafIndex(environment, "request_events", "request_events_global_cursor_idx", "global_cursor", "event_at ASC, event_id ASC");
  ensureLeafIndex(environment, "request_events", "request_events_tenant_cursor_idx", "tenant_cursor", "tenant_id, event_at ASC, event_id ASC");
}

function selectedDays(environment: NodeJS.ProcessEnv, table: TableName, options: Options): string[] {
  const column = timeColumn(table);
  const predicate = datePredicate(`to_timestamp(${column} / 1000.0)`, options.fromDate);
  return captured(environment, { from_date: options.fromDate ?? "", before_date: options.beforeDate }, `SELECT to_char(to_timestamp(${column} / 1000.0) AT TIME ZONE 'UTC', 'YYYY-MM-DD')
  FROM public.${table}_default
 WHERE ${predicate}
 GROUP BY 1
 ORDER BY 1
 LIMIT ${options.maxDays};
`).split(/\r?\n/u).filter(Boolean);
}

function backfillDay(environment: NodeJS.ProcessEnv, table: TableName, day: string, batchSize: number): void {
  console.log(`Backfilling ${table} UTC day ${day} in committed batches of ${batchSize}`);
  psql(environment, {
    args: variables({ table_name: table, day, batch_size: batchSize }),
    input: `SELECT pg_try_advisory_lock(
           hashtext('memeloop-token-center-history-partition'),
           hashtext(:'table_name')
       ) AS acquired \\gset
\\if :acquired
CALL public.mtc_backfill_history_partition(
    :'table_name', :'day'::date, :batch_size
);
SELECT pg_advisory_unlock(
           hashtext('memeloop-token-center-history-partition'),
           hashtext(:'table_name')
       );
\\else
\\echo 'another history backfill holds the PostgreSQL advisory lock'
\\quit 75
\\endif
`,
  });
  const target = `${table}_${day.replaceAll("-", "")}`;
  psql(environment, { args: variables({ target }), input: 'ANALYZE public.:"target";\n' });
  psql(environment, {
    args: variables({ table_name: table, day }),
    input: `SELECT table_name, day_start, status, source_rows, staged_rows, moved_rows,
       source_rows = staged_rows AND staged_rows = moved_rows AS counts_match,
       completed_at
  FROM public.mtc_history_partition_backfill_state
 WHERE table_name = :'table_name' AND day_start = :'day'::date;
`,
  });
}

export function main(argv = process.argv.slice(2)): number {
  try {
    const options = parseArguments(argv);
    if (options === undefined) {
      process.stdout.write(usage);
      return 0;
    }
    const environment = requiredLibpqEnvironment();
    const postgresDirectory = resolve(dirname(fileURLToPath(import.meta.url)), "postgres");
    if (options.action === "dry-run") {
      dryRun(environment, options);
      return 0;
    }
    console.log("APPLY mode selected. Each daily cutover is atomic and independently restartable.");
    psql(environment, { args: ["-f", resolve(postgresDirectory, "history-partition-backfill.sql")] });
    installIndexes(environment, postgresDirectory);
    if (options.indexesOnly) {
      console.log("Index installation and verification complete; no rows were moved.");
      return 0;
    }
    for (const table of tableNames(options.tableSelection)) {
      const days = selectedDays(environment, table, options);
      if (days.length === 0) {
        console.log(`No selected completed UTC days remain in public.${table}_default.`);
        continue;
      }
      for (const day of days) backfillDay(environment, table, day, options.batchSize);
    }
    console.log("Apply run complete. Run again without --apply to inspect remaining default-partition rows.");
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
