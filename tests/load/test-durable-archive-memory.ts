import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import test from "node:test";
import { inputPlan, memoryVerdict, outputPlan, permitEvidence, planHash } from "../../ops/benchmark-durable-archive-memory.ts";

test("request and response plans have exact wire sizes and valid JSON", () => {
  for (const plan of [inputPlan(1024), outputPlan(2048)]) {
    const body = Buffer.concat([plan.prefix, Buffer.alloc(plan.fillBytes, "x"), plan.suffix]);
    assert.equal(body.length, plan.bytes);
    assert.equal(typeof JSON.parse(body.toString("utf8")), "object");
    assert.equal(planHash(plan), createHash("sha256").update(body).digest("hex"));
  }
  assert.equal(inputPlan(16 * 1024 * 1024).bytes, 16 * 1024 * 1024);
  assert.equal(outputPlan(64 * 1024 * 1024).bytes, 64 * 1024 * 1024);
});

test("RSS acceptance requires real peak cap AND retained-memory recovery", () => {
  assert.equal(memoryVerdict(80, 440, 100), true);
  assert.equal(memoryVerdict(80, 449, 100), false);
  assert.equal(memoryVerdict(80, 440, 200), false);
  assert.equal(memoryVerdict(80, 120, 120), false);
});

test("missing permit telemetry never masquerades as returned permits", () => {
  assert.throws(() => permitEvidence(""), /required permit gauge absent/u);
});

test("CI reuses its exact optimized binary and retains kernel RSS evidence", () => {
  const workflow = readFileSync(new URL("../../.github/workflows/memory-acceptance.yml", import.meta.url), "utf8");
  const harness = readFileSync(new URL("../../ops/benchmark-durable-archive-memory.ts", import.meta.url), "utf8");
  assert.match(workflow, /node ops\/benchmark-durable-archive-memory\.ts\s+\\\s+target\/release\/memeloop-token-center/u);
  assert.match(workflow, /DURABLE_RSS_EXIT_CODE/u);
  assert.match(harness, /processMemory\(service/u);
  assert.match(harness, /high_water_mib/u);
  assert.match(harness, /mock_pid = process\.pid/u);
  assert.doesNotMatch(harness, /cargo\s+build/u);
});
