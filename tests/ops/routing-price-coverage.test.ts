import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import test from "node:test";

import { coverageExitCode, parseArguments } from "../../scripts/maintenance/check-routing-price-coverage.ts";

const driver = new URL("../../scripts/maintenance/check-routing-price-coverage.ts", import.meta.url);
const complete = {
  contract: "474-actual-upstream-price-v1", read_only: true,
  candidate_count: 2, checked_pair_count: 4, gap_count: 0, gap_samples: [],
};

test("coverage checks never accept mutation, arbitrary SQL or currency injection", () => {
  assert.deepEqual(parseArguments([]), []);
  assert.deepEqual(parseArguments(["--currency", "usd", "--currency", "CNY", "--currency", "USD"]), ["CNY", "USD"]);
  for (const arguments_ of [["--apply"], ["--currency"], ["--currency", "USD';DELETE"], ["--sql", "SELECT 1"]]) {
    assert.throws(() => parseArguments(arguments_));
  }
});

test("gaps, unknown currencies, empty scopes and malformed reports cannot PASS", () => {
  assert.equal(coverageExitCode(complete), 0);
  assert.equal(coverageExitCode({ ...complete, gap_count: 1 }), 2);
  assert.equal(coverageExitCode({ ...complete, candidate_count: 0, checked_pair_count: 0 }), 2);
  for (const report of [null, {}, { ...complete, read_only: false }, { ...complete, gap_count: -1 }, { ...complete, checked_pair_count: 1 }]) {
    assert.throws(() => coverageExitCode(report));
  }
});

test("CLI help is offline and diagnostics never echo libpq credentials", () => {
  const help = spawnSync(process.execPath, [driver.pathname, "--help"], { encoding: "utf8", env: {}, shell: false });
  assert.equal(help.status, 0);
  assert.match(help.stdout, /Read-only PostgreSQL/u);
  const secret = "do-not-echo-db-secret";
  const failed = spawnSync(process.execPath, [driver.pathname], {
    encoding: "utf8", shell: false,
    env: { PATH: "", PGHOST: secret, PGUSER: secret, PGDATABASE: secret, PGPASSWORD: secret },
  });
  assert.equal(failed.status, 1);
  assert.ok(!`${failed.stdout}${failed.stderr}`.includes(secret));
});

test("SQL is bounded, rollback-only, exact-model and includes failover/group candidates", () => {
  const sql = readFileSync(new URL("../../scripts/maintenance/check-routing-price-coverage.sql", import.meta.url), "utf8");
  assert.match(sql, /^BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;/u);
  assert.match(sql, /ROLLBACK;\s*$/u);
  assert.doesNotMatch(sql, /\b(?:INSERT|UPDATE|DELETE|ALTER|CREATE|COMMIT|FOR UPDATE)\b/iu);
  for (const marker of ["statement_timeout", "lock_timeout", "model_route_upstream_accounts", "model_route_included_provider_groups", "model_route_excluded_provider_groups", "price.model = candidate.upstream_model", "model_price_tiers", "currency_scope_unknown", "LIMIT 200"]) {
    assert.ok(sql.includes(marker), marker);
  }
});
