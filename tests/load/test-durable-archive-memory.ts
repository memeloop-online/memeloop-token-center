import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import test from "node:test";
import { allocatorEvidence, inputPlan, memoryVerdict, outputPlan, permitEvidence, planHash, requestPath, responsesInputPlan, responsesOutputPlan } from "../../ops/benchmark-durable-archive-memory.ts";

test("request and response plans have exact wire sizes and valid JSON", () => {
  for (const plan of [inputPlan(1024), outputPlan(2048), responsesInputPlan(1024), responsesOutputPlan(2048)]) {
    const body = Buffer.concat([plan.prefix, Buffer.alloc(plan.fillBytes, "x"), plan.suffix]);
    assert.equal(body.length, plan.bytes);
    assert.equal(typeof JSON.parse(body.toString("utf8")), "object");
    assert.equal(planHash(plan), createHash("sha256").update(body).digest("hex"));
  }
  assert.equal(inputPlan(16 * 1024 * 1024).bytes, 16 * 1024 * 1024);
  assert.equal(outputPlan(64 * 1024 * 1024).bytes, 64 * 1024 * 1024);
});

test("16MiB pressure uses the real Responses endpoint and wire contract", () => {
  assert.equal(requestPath(responsesInputPlan(16 * 1024 * 1024)), "/v1/responses");
  assert.equal(requestPath(inputPlan(512)), "/v1/chat/completions");
  const request = responsesInputPlan(1024);
  const parsed = JSON.parse(Buffer.concat([request.prefix, Buffer.alloc(request.fillBytes, "x"), request.suffix]).toString());
  assert.equal(typeof parsed.input, "string");
  assert.equal(parsed.stream, false);
  assert.equal(parsed.messages, undefined);
  const response = responsesOutputPlan(1024);
  const result = JSON.parse(Buffer.concat([response.prefix, Buffer.alloc(response.fillBytes, "x"), response.suffix]).toString());
  assert.equal(result.status, "completed");
  assert.equal(result.usage.input_tokens, 2);
  assert.equal(result.output[0].content[0].type, "output_text");
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

test("allocator evidence preserves every jemalloc state", () => {
  const metrics = ["allocated", "active", "resident", "mapped", "retained"]
    .map((state, index) => `memeloop_token_center_allocator_bytes{state="${state}"} ${(index + 1) * 1024}`)
    .join("\n");
  assert.deepEqual(allocatorEvidence(metrics), {
    allocated: 1024,
    active: 2048,
    resident: 3072,
    mapped: 4096,
    retained: 5120,
  });
  assert.throws(() => allocatorEvidence(metrics.replace(/^.*retained.*$/mu, "")), /required allocator gauge absent: retained/u);
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
