#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const usage = `Usage: check-routing-price-coverage.ts [--currency USD|CNY]... [--help]

Read-only PostgreSQL price coverage preflight for PR474, not a request gate.
Uses PGHOST, PGUSER, PGDATABASE and libpq authentication (PGPASSFILE recommended).
Checks every configured candidate of enabled text routes in active tenants,
including failover and catalog-dormant candidates, for tenant key/account
currencies. --currency adds planned currencies across all tenants.
Never prints credentials, model names, aliases, rates, or request data.
Exit 0: complete coverage; 2: gaps or empty scope; 1: check could not complete.
No writes, migrations, price copying, or route disabling are supported.
`;

export function parseArguments(arguments_: string[]): string[] | undefined {
  if (arguments_.length === 1 && ["--help", "-h"].includes(arguments_[0]!)) return undefined;
  const currencies = new Set<string>();
  for (let index = 0; index < arguments_.length; index += 2) {
    const currency = arguments_[index + 1]?.toUpperCase();
    if (arguments_[index] !== "--currency" || !currency || !["USD", "CNY"].includes(currency)) {
      throw new Error("expected --currency USD|CNY; no mutation or custom SQL options exist");
    }
    currencies.add(currency);
  }
  return [...currencies].sort();
}

export function coverageExitCode(report: unknown): number {
  if (typeof report !== "object" || report === null) throw new Error("invalid coverage report");
  const checked = report as Record<string, unknown>;
  for (const field of ["candidate_count", "checked_pair_count", "gap_count"]) {
    const count = checked[field];
    if (typeof count !== "number" || !Number.isSafeInteger(count) || count < 0) {
      throw new Error("invalid coverage counts");
    }
  }
  if (checked.contract !== "474-actual-upstream-price-v1" || checked.read_only !== true
      || !Array.isArray(checked.gap_samples)
      || (checked.gap_count as number) > (checked.checked_pair_count as number)
      || (checked.checked_pair_count as number) < (checked.candidate_count as number)) {
    throw new Error("incomplete or non-read-only coverage report");
  }
  return checked.candidate_count === 0 || checked.gap_count !== 0 ? 2 : 0;
}

function main(): number {
  const currencies = parseArguments(process.argv.slice(2));
  if (currencies === undefined) {
    process.stdout.write(usage);
    return 0;
  }
  for (const name of ["PGHOST", "PGUSER", "PGDATABASE"]) {
    if (!process.env[name]) throw new Error("PGHOST, PGUSER and PGDATABASE are required");
  }
  const result = spawnSync("psql", [
    "-X", "--no-psqlrc", "--no-password", "--quiet", "--tuples-only", "--no-align",
    "-v", "ON_ERROR_STOP=1", "-v", `currencies=${JSON.stringify(currencies)}`,
  ], {
    input: readFileSync(new URL("./check-routing-price-coverage.sql", import.meta.url), "utf8"),
    encoding: "utf8",
    env: { ...process.env, PGCONNECT_TIMEOUT: "10", PGAPPNAME: "mtc-price-coverage-readonly" },
    shell: false,
    timeout: 75_000,
    maxBuffer: 1024 * 1024,
  });
  if (result.error || result.status !== 0) {
    throw new Error("price coverage query failed or timed out; no PASS (database diagnostics withheld)");
  }
  let report: unknown;
  try {
    report = JSON.parse(result.stdout.trim());
  } catch {
    throw new Error("price coverage output invalid; no PASS");
  }
  const exitCode = coverageExitCode(report);
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  return exitCode;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    process.exitCode = main();
  } catch {
    process.stderr.write("Price coverage preflight incomplete; no PASS. Check arguments, read-only access and schema.\n");
    process.exitCode = 1;
  }
}
