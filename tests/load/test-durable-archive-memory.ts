import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import test from "node:test";
import { allocatorEvidence, inputPlan, memoryVerdict, nativeAllocatorEvidence, outputPlan, permitEvidence, planHash, requestPath, responsesInputPlan, responsesOutputPlan } from "../../ops/benchmark-durable-archive-memory.ts";
import { processMemoryFromProc } from "../../ops/benchmark-memory.ts";

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

test("native allocator and proc attribution preserve non-jemalloc RSS evidence", () => {
  const metrics = ["arena", "allocated", "free", "mmap", "releasable"]
    .map((state, index) => `memeloop_token_center_native_allocator_bytes{state="${state}"} ${(index + 1) * 2048}`)
    .join("\n");
  assert.deepEqual(nativeAllocatorEvidence(metrics), {
    arena: 2048,
    allocated: 4096,
    free: 6144,
    mmap: 8192,
    releasable: 10240,
  });
  assert.throws(() => nativeAllocatorEvidence(metrics.replace(/^.*mmap.*$/mu, "")), /required native allocator gauge absent: mmap/u);

  const status = "VmRSS:\t122880 kB\nVmHWM:\t358400 kB\nRssAnon:\t102400 kB\nRssFile:\t18432 kB\nRssShmem:\t2048 kB\nVmData:\t110592 kB\nVmSwap:\t1024 kB\n";
  const smaps = "Pss:\t116736 kB\nPss_Anon:\t100352 kB\nPss_File:\t15360 kB\nPss_Shmem:\t1024 kB\nPrivate_Clean:\t4096 kB\nPrivate_Dirty:\t101376 kB\nShared_Clean:\t16384 kB\nShared_Dirty:\t1024 kB\nAnonymous:\t103424 kB\n";
  assert.deepEqual(processMemoryFromProc(status, smaps), {
    rss_mib: 120,
    high_water_mib: 350,
    pss_mib: 114,
    rss_anon_mib: 100,
    rss_file_mib: 18,
    rss_shmem_mib: 2,
    data_mib: 108,
    swap_mib: 1,
    pss_anon_mib: 98,
    pss_file_mib: 15,
    pss_shmem_mib: 1,
    private_clean_mib: 4,
    private_dirty_mib: 99,
    shared_clean_mib: 16,
    shared_dirty_mib: 1,
    anonymous_mib: 101,
  });
});

test("CI reuses its exact optimized binary and retains kernel RSS evidence", () => {
  const workflow = readFileSync(new URL("../../.github/workflows/memory-acceptance.yml", import.meta.url), "utf8");
  const harness = readFileSync(new URL("../../ops/benchmark-durable-archive-memory.ts", import.meta.url), "utf8");
  assert.match(workflow, /node ops\/benchmark-durable-archive-memory\.ts\s+\\\s+target\/release\/memeloop-token-center/u);
  assert.match(workflow, /DURABLE_RSS_EXIT_CODE/u);
  assert.match(harness, /processMemory\(service/u);
  assert.match(harness, /high_water_mib/u);
  assert.match(harness, /native_allocator_bytes/u);
  assert.match(harness, /process_memory/u);
  assert.match(harness, /mock_pid = process\.pid/u);
  assert.doesNotMatch(harness, /cargo\s+build/u);
});
