#!/usr/bin/env node
/** Replay-safe CPAMP SQLite to PostgreSQL importer. */

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import {
  accessSync,
  constants as fsConstants,
  lstatSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));

class CliError extends Error {
  readonly exitCode: number;

  constructor(message: string, exitCode = 2) {
    super(message);
    this.exitCode = exitCode;
  }
}

function fail(message: string): never {
  throw new CliError(message);
}

function required(name: string): string {
  const value = process.env[name];
  if (!value) fail(`${name} is required`);
  return value;
}

function booleanSetting(name: string, fallback: boolean): boolean {
  const raw = process.env[name] ?? String(fallback);
  if (raw !== "true" && raw !== "false") fail(`${name} must be true or false`);
  return raw === "true";
}

function unsignedSetting(name: string, fallback: string): bigint {
  const raw = process.env[name] ?? fallback;
  if (!/^\d+$/.test(raw)) fail(`${name} must be an integer`);
  return BigInt(raw);
}

function psqlArgs(extra: readonly string[] = []): string[] {
  return ["-X", "-v", "ON_ERROR_STOP=1", "--no-psqlrc", ...extra];
}

function runPsql(
  extra: readonly string[],
  input?: string,
  output: "capture" | "inherit" | "ignore" = "inherit",
): string {
  const result = spawnSync("psql", psqlArgs(extra), {
    encoding: "utf8",
    env: process.env,
    input,
    shell: false,
    stdio: [input === undefined ? "inherit" : "pipe", output === "capture" ? "pipe" : output, "inherit"],
  });
  if (result.error) fail(`psql is unavailable: ${result.error.message}`);
  if (result.status !== 0) fail("PostgreSQL command failed");
  return output === "capture" ? String(result.stdout).trim() : "";
}

async function pipeSqliteCsvToPostgres(sqlitePath: string, query: string, table: string): Promise<void> {
  const sqlite = spawn("sqlite3", ["-header", "-csv", sqlitePath, query], {
    shell: false,
    stdio: ["ignore", "pipe", "inherit"],
  });
  const postgres = spawn("psql", psqlArgs(["-c", `\\copy ${table} FROM STDIN WITH (FORMAT csv, HEADER true)`]), {
    env: process.env,
    shell: false,
    stdio: ["pipe", "inherit", "inherit"],
  });
  sqlite.stdout.pipe(postgres.stdin);
  const result = await Promise.all([
    new Promise<number>((resolve, reject) => {
      sqlite.once("error", reject);
      sqlite.once("close", (code) => resolve(code ?? 1));
    }),
    new Promise<number>((resolve, reject) => {
      postgres.once("error", reject);
      postgres.once("close", (code) => resolve(code ?? 1));
    }),
  ]).catch((error: unknown) => {
    sqlite.kill("SIGTERM");
    postgres.kill("SIGTERM");
    fail(`SQLite/PostgreSQL streaming copy failed: ${error instanceof Error ? error.message : "unknown error"}`);
  });
  if (result[0] !== 0 || result[1] !== 0) fail("SQLite/PostgreSQL streaming copy failed");
}

function readSql(name: string): string {
  return readFileSync(join(scriptDirectory, "sql", "cpamp", name), "utf8");
}

async function sleep(milliseconds: number): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function main(): Promise<void> {
  const pgHost = required("PGHOST");
  const pgPort = process.env.PGPORT ?? "5432";
  const pgUser = required("PGUSER");
  const pgDatabase = required("PGDATABASE");
  const sqlitePath = process.env.CPAMP_SQLITE_PATH ?? "/source/usage.sqlite";
  const tenant = process.env.IMPORT_TENANT_EXTERNAL_ID ?? "cpa-dogfood-import";
  const source = process.env.CPAMP_IMPORT_SOURCE ?? "cpamp-usage-events-v1";
  const overlapMs = unsignedSetting("CPAMP_OVERLAP_MS", "86400000");
  const reset = booleanSetting("CPAMP_RESET_IMPORT", false);
  const allowUnmapped = booleanSetting("CPAMP_ALLOW_UNMAPPED", false);

  if (reset && tenant !== "cpa-dogfood-import") fail("reset is only allowed for the cpa-dogfood-import tenant");
  if (reset && process.env.CPAMP_RESET_CONFIRM !== "DELETE_CPA_DOGFOOD_IMPORT") {
    fail("CPAMP_RESET_CONFIRM=DELETE_CPA_DOGFOOD_IMPORT is required for a reset");
  }
  try {
    accessSync(sqlitePath, fsConstants.R_OK);
  } catch {
    fail("CPAMP SQLite database is not readable");
  }
  if (!/^[A-Za-z0-9._:-]+$/.test(source)) fail("CPAMP_IMPORT_SOURCE contains unsupported characters");

  const pgPassFile = process.env.PGPASSFILE;
  const pgPassword = process.env.PGPASSWORD;
  if (pgPassFile && pgPassword) fail("set exactly one of PGPASSFILE or PGPASSWORD");
  if (pgPassFile) {
    try {
      const metadata = lstatSync(pgPassFile);
      accessSync(pgPassFile, fsConstants.R_OK);
      if (!metadata.isFile() || metadata.isSymbolicLink()) throw new Error("not a regular file");
      if ((metadata.mode & 0o777) !== 0o600) fail("PGPASSFILE must have mode 0600");
    } catch (error) {
      if (error instanceof Error && error.message === "PGPASSFILE must have mode 0600") throw error;
      fail("PGPASSFILE must be a readable regular non-symlink file");
    }
  } else if (!pgPassword) {
    fail("PGPASSFILE or PGPASSWORD is required");
  }
  Object.assign(process.env, { PGHOST: pgHost, PGPORT: pgPort, PGUSER: pgUser, PGDATABASE: pgDatabase });

  const lockKey = "memeloop-token-center:cpamp:global-staging-v1";
  const lockDirectory = mkdtempSync(join(tmpdir(), "mtc-cpamp-lock."));
  const lockApplication = `mtc-cpamp-lock-${lockDirectory.slice(lockDirectory.lastIndexOf(".") + 1)}`;
  let lockProcess: ChildProcess | undefined;
  let lockSpawnError: Error | undefined;

  const cleanup = (): void => {
    if (lockProcess?.exitCode === null) lockProcess.kill("SIGTERM");
    try {
      runPsql(["-v", `lock_app=${lockApplication}`, "-At"], `SELECT pg_terminate_backend(pid)\n  FROM pg_stat_activity\n WHERE application_name = :'lock_app'\n   AND usename = current_user\n   AND pid <> pg_backend_pid();\n`, "ignore");
    } catch {
      // Cleanup is best effort; the importing command has already failed or completed.
    }
    rmSync(lockDirectory, { recursive: true, force: true });
  };
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
    process.once(signal, () => {
      cleanup();
      process.exit(128 + ({ SIGHUP: 1, SIGINT: 2, SIGTERM: 15 } as const)[signal]);
    });
  }

  try {
    lockProcess = spawn("psql", psqlArgs(["-v", `lock_key=${lockKey}`, "-At"]), {
      env: { ...process.env, PGAPPNAME: lockApplication },
      shell: false,
      stdio: ["pipe", "ignore", "inherit"],
    });
    lockProcess.once("error", (error) => {
      lockSpawnError = error;
    });
    lockProcess.stdin?.once("error", (error) => {
      lockSpawnError = error;
    });
    lockProcess.stdin?.end("SET lock_timeout = '30s';\nSELECT pg_advisory_lock(hashtextextended(:'lock_key', 734627102948313));\nSELECT pg_sleep(86400);\n");

    let lockAcquired = false;
    for (let attempt = 0; attempt < 30; attempt += 1) {
      const ready = runPsql(
        ["-v", `lock_app=${lockApplication}`, "-At"],
        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE application_name = :'lock_app' AND wait_event = 'PgSleep');\n",
        "capture",
      );
      if (ready === "t") {
        lockAcquired = true;
        break;
      }
      if (lockSpawnError) fail(`psql is unavailable: ${lockSpawnError.message}`);
      if (lockProcess.exitCode !== null) break;
      await sleep(1_000);
    }
    if (!lockAcquired) fail("another CPAMP import is running or the import lock failed");

    runPsql(["-v", `tenant_external_id=${tenant}`], readSql("prepare.sql"));
    if (reset) {
      runPsql(["-v", `tenant_external_id=${tenant}`, "-v", `import_source=${source}`], readSql("reset.sql"));
    }

    const watermarkText = runPsql(
      ["-v", `tenant_external_id=${tenant}`, "-v", `import_source=${source}`, "-At"],
      "SELECT COALESCE((SELECT watermark_ms FROM cpamp_import_checkpoints WHERE tenant_external_id = :'tenant_external_id' AND source = :'import_source'), 0);\n",
      "capture",
    );
    if (!/^\d+$/.test(watermarkText)) fail("invalid PostgreSQL import watermark");
    const watermark = BigInt(watermarkText);
    const lowerBound = watermark > overlapMs ? watermark - overlapMs : 0n;

    await pipeSqliteCsvToPostgres(
      sqlitePath,
      `SELECT event_hash, request_id, timestamp_ms, provider, model, endpoint, lower(api_key_hash), input_tokens, output_tokens, latency_ms, CASE WHEN failed THEN 1 ELSE 0 END, COALESCE(fail_status_code, 0), COALESCE(fail_summary, '') FROM usage_events WHERE event_hash <> '' AND timestamp_ms >= ${lowerBound};`,
      "cpamp_import_usage",
    );
    await pipeSqliteCsvToPostgres(sqlitePath, "SELECT lower(api_key_hash), alias, updated_at_ms FROM api_key_aliases;", "cpamp_import_aliases");
    await pipeSqliteCsvToPostgres(sqlitePath, "SELECT model, prompt_per_1m, completion_per_1m, source, updated_at_ms FROM model_prices;", "cpamp_import_prices");

    const validationSql = `SELECT count(*) FROM cpamp_import_usage;\nSELECT count(*) FROM cpamp_import_usage WHERE COALESCE(api_key_hash, '') !~ '^[0-9a-f]{64}$';\nSELECT count(*) FROM cpamp_import_aliases WHERE COALESCE(api_key_hash, '') !~ '^[0-9a-f]{64}$';\nSELECT COALESCE(sum(events - 1), 0) FROM (SELECT count(*) AS events FROM cpamp_import_usage GROUP BY event_hash HAVING count(*) > 1) duplicates;\nWITH duplicate_event_hashes AS MATERIALIZED (SELECT event_hash FROM cpamp_import_usage GROUP BY event_hash HAVING count(*) > 1), digested_duplicates AS MATERIALIZED (SELECT u.event_hash, encode(sha256(convert_to(jsonb_build_array(u.request_id, u.timestamp_ms, u.provider, u.model, u.endpoint, u.api_key_hash, u.input_tokens, u.output_tokens, u.latency_ms, u.failed, u.fail_status_code, u.fail_summary)::text, 'UTF8')), 'hex') AS source_digest FROM cpamp_import_usage u JOIN duplicate_event_hashes d ON d.event_hash = u.event_hash) SELECT count(*) FROM (SELECT event_hash FROM digested_duplicates GROUP BY event_hash HAVING count(DISTINCT source_digest) > 1) conflicts;\n`;
    const values = runPsql(["-At"], validationSql, "capture").split(/\s+/);
    if (values.length !== 5 || values.some((value) => !/^\d+$/.test(value ?? ""))) fail("invalid CPAMP staging validation result");
    const [stagedText, unmappedText, invalidAliasText, duplicateText, conflictsText] = values as [string, string, string, string, string];
    const unmapped = BigInt(unmappedText);
    const invalidAliases = BigInt(invalidAliasText);
    const conflicts = BigInt(conflictsText);
    if (unmapped > 0n && !allowUnmapped) fail(`CPAMP import stopped: ${unmappedText} staged events have no supported key identity; set CPAMP_ALLOW_UNMAPPED=true only after accepting that data loss`);
    if (invalidAliases > 0n) fail(`CPAMP import stopped: ${invalidAliasText} staged aliases have a non-hex key identity`);
    if (conflicts > 0n) fail(`CPAMP import stopped: ${conflictsText} event hashes map to conflicting source rows`);
    process.stderr.write(`CPAMP staged=${stagedText} unmapped=${unmappedText} duplicate_rows_deduplicated=${duplicateText} conflicting_event_hashes=0\n`);

    runPsql(["-v", `tenant_external_id=${tenant}`, "-v", `import_source=${source}`], readSql("apply.sql"));
  } finally {
    cleanup();
  }
}

try {
  await main();
} catch (error) {
  if (error instanceof CliError) {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = error.exitCode;
  } else {
    process.stderr.write(`CPAMP import failed: ${error instanceof Error ? error.message : "unknown error"}\n`);
    process.exitCode = 1;
  }
}
