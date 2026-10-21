import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import test from "node:test";
import { allocatorEvidence, archiveDrainReady, assertArchiveGapEvidence, assertUnknownFinancialEvidence, assertUnknownSemanticEvidence, deferredPersistenceEvidence, requestPersistenceEvidence, inputPlan, memoryVerdict, nativeAllocatorEnvironment, nativeAllocatorEvidence, NATIVE_MMAP_THRESHOLD_BYTES, outputPlan, permitEvidence, planHash, requestPath, responsesInputPlan, responsesOutputPlan } from "./benchmark-durable-archive-memory.ts";
import { processMemoryFromProc } from "./benchmark-memory.ts";

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

test("drain exempts only one explicitly owned unknown hold and its exact durable archive", () => {
  const empty = { pending_requests: 0, unsettled_reservations: 0, cipher_bytes: 0, request_cipher_bytes: 0 };
  assert.equal(archiveDrainReady(empty), true);
  const retained = { pending_requests: 1, unsettled_reservations: 1, cipher_bytes: 2048, request_cipher_bytes: 2048 };
  assert.equal(archiveDrainReady(retained), false);
  assert.equal(archiveDrainReady(retained, 2048), true);
  assert.equal(archiveDrainReady(empty, 2048), false);
  for (const field of Object.keys(retained)) {
    assert.equal(archiveDrainReady({ ...retained, [field]: retained[field as keyof typeof retained] + 1 }, 2048), false);
    assert.equal(archiveDrainReady({ ...retained, [field]: 0 }, 2048), false);
  }
  assert.throws(() => archiveDrainReady(retained, NaN), /retained archive bytes/u);
  assert.throws(() => archiveDrainReady(retained, -1), /retained archive bytes/u);
});

test("unknown requires owned semantics exactly once while retaining its receipt, hold and no financial ACK", () => {
  const responseObject = 'inline-json:{"error":{"message":"upstream request failed","type":"upstream_error"}}';
  const claim = {
    request_id: "request", tenant_id: "tenant", key_id: "key", account_id: "account",
    reservation_id: "reservation", owner_token: "owner", fence: "fence", contract_digest: "digest",
  };
  const row = {
    ...claim, financial_completed: null, financial_status: null, actual_micros: null,
    receipt_id: null, receipt_digest: null, cost_micros: null, reserved_micros: 123,
    reservation_status: "reserved", state: "pending", unknown_reason: "authoritative_usage_unavailable",
    ledger_count: 0, event_count: 0, fact_count: 0, observation_count: 1, projected_event_count: 1,
    principal_id: "principal", request_cluster_id: "cluster",
    observation_id: "observation", observation_request_id: "request", observation_key_id: "key",
    observation_cluster_id: "cluster", cluster_tenant_id: "tenant", cluster_principal_id: "principal",
    projection_tenant_id: "tenant", projection_key_id: "key", projection_principal_id: "principal",
    projection_completed: 1235, projection_observed_at: 1234, projected_event_id: "projected",
    projected_event_tenant_id: "tenant", projected_event_key_id: "key",
    operational_status: 502, operational_error: "upstream_invalid_response",
    operational_stage: "buffered_response_ready", operational_completed: 1234,
    operational_receipt_id: "receipt", operational_response_object: responseObject,
    spool_state: "pending", spool_plain_bytes: 512, chunk_plain_bytes: 512, spool_cipher_bytes: 2048,
  };
  const receipt = {
    request_id: "request", receipt_id: "receipt", usage_basis: "not_observed",
    status_code: 502, error_code: "upstream_invalid_response", response_object: responseObject,
    claim, reservation: { id: "reservation", reserved_micros: 123 },
    operational_terminal: {
      status_code: 502, error_code: "upstream_invalid_response", stage: "buffered_response_ready",
      completed_at: 1234, response_object: responseObject,
    },
  };
  assert.equal(assertUnknownFinancialEvidence(row, receipt), 2048);
  const semantics = assertUnknownSemanticEvidence(row);
  assert.deepEqual(assertUnknownSemanticEvidence({ ...row }, semantics), semantics);
  assert.equal(assertUnknownFinancialEvidence({ ...row }, receipt), 2048);
  for (const count of [0, 2]) {
    assert.throws(() => assertUnknownSemanticEvidence({ ...row, observation_count: count }), /exactly one conversation observation/u);
    assert.throws(() => assertUnknownSemanticEvidence({ ...row, projected_event_count: count }), /exactly one semantic projected event/u);
    assert.throws(() => assertUnknownSemanticEvidence({ ...row, observation_count: count }, semantics), /exactly one conversation observation/u);
    assert.throws(() => assertUnknownSemanticEvidence({ ...row, projected_event_count: count }, semantics), /exactly one semantic projected event/u);
  }
  for (const field of [
    "observation_request_id", "observation_key_id", "cluster_tenant_id", "cluster_principal_id",
    "observation_cluster_id", "projection_tenant_id", "projection_key_id", "projection_principal_id",
    "projected_event_tenant_id", "projected_event_key_id",
  ]) {
    assert.throws(() => assertUnknownSemanticEvidence({ ...row, [field]: "other-owner" }), /exact ownership/u);
  }
  assert.throws(() => assertUnknownSemanticEvidence({ ...row, projection_completed: null }), /projection must commit/u);
  assert.throws(() => assertUnknownSemanticEvidence({ ...row, projection_observed_at: 999 }, semantics), /operational receipt timestamp/u);
  assert.throws(() => assertUnknownSemanticEvidence({ ...row, projection_completed: 1236 }, semantics), /replay must preserve timestamp/u);
  assert.throws(() => assertUnknownSemanticEvidence({ ...row, operational_completed: 1235, projection_observed_at: 1235 }, semantics), /replay must preserve timestamp/u);
  for (const field of ["observation_id", "projected_event_id", "operational_receipt_id"]) {
    assert.throws(() => assertUnknownSemanticEvidence({ ...row, [field]: "" }), /semantic evidence missing/u);
    assert.throws(() => assertUnknownSemanticEvidence({ ...row, [field]: "replacement" }, semantics), /replay must preserve identity/u);
  }
  assert.throws(() => assertUnknownSemanticEvidence({ ...row, observation_cluster_id: "replacement", request_cluster_id: "replacement" }, semantics), /replay must preserve identity/u);
  for (const field of ["financial_completed", "financial_status", "actual_micros", "receipt_id", "receipt_digest", "cost_micros"]) {
    assert.throws(() => assertUnknownFinancialEvidence({ ...row, [field]: 0 }, receipt), /financial completion/u);
  }
  for (const field of ["ledger_count", "event_count", "fact_count"]) {
    assert.throws(() => assertUnknownFinancialEvidence({ ...row, [field]: 1 }, receipt), /financial side effects/u);
  }
  for (const field of Object.keys(claim)) {
    assert.throws(() => assertUnknownFinancialEvidence(row, { ...receipt, claim: { ...claim, [field]: "different" } }), /claim authority/u);
  }
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, reservation_status: "settled" }, receipt), /original financial hold/u);
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, operational_status: 499 }, receipt), /real upstream failure/u);
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, operational_completed: null }, receipt), /completion must be recorded/u);
  assert.throws(() => assertUnknownFinancialEvidence(row, { ...receipt, receipt_id: "different" }), /operational receipt/u);
  assert.throws(() => assertUnknownFinancialEvidence(row, { ...receipt, usage_basis: "provider_reported" }), /fabricate provider usage/u);
  assert.throws(() => assertUnknownFinancialEvidence(row, { ...receipt, reservation: { id: "reservation", reserved_micros: 0 } }), /original reservation/u);
  assert.throws(() => assertUnknownFinancialEvidence(row, { ...receipt, operational_terminal: { ...receipt.operational_terminal, completed_at: 1235 } }), /operational ACK/u);
  assert.throws(() => assertUnknownFinancialEvidence(row, { ...receipt, response_object: "inline-json:private upstream body" }), /safe local error/u);
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, spool_state: "gap" }, receipt), /remain durable/u);
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, chunk_plain_bytes: 0 }, receipt), /remain durable/u);
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, spool_cipher_bytes: 0 }, receipt), /positive accounting/u);
});

test("archive gaps require explicit finite capacity evidence", () => {
  const metrics = ["accepted", "capacity", "failed"]
    .map((outcome, index) => `memeloop_token_center_deferred_persistence_total{outcome="${outcome}"} ${index}`)
    .join("\n");
  assert.deepEqual(deferredPersistenceEvidence(metrics), { accepted: 0, capacity: 1, failed: 2 });
  assert.throws(() => deferredPersistenceEvidence(""), /required deferred persistence counter absent/u);
  assert.throws(() => deferredPersistenceEvidence(metrics.replace("} 1", "} NaN")), /invalid deferred persistence counter/u);
});

test("request and response gaps require their own bounded queue evidence", () => {
  const request = { accepted: 1, capacity: 2, failed: 0, retention_limit: 0 };
  const response = { accepted: 2, capacity: 1, failed: 0 };
  const metrics = Object.entries(request).map(([outcome, value]) => `memeloop_token_center_request_persistence_total{outcome="${outcome}"} ${value}`).join("\n");
  assert.deepEqual(requestPersistenceEvidence(metrics), request);
  assert.throws(() => requestPersistenceEvidence(""), /required request persistence counter absent/u);
  assert.throws(() => requestPersistenceEvidence(metrics.replace("} 2", "} NaN")), /invalid request persistence counter/u);
  assertArchiveGapEvidence(2, 1, request, response);
  assert.throws(() => assertArchiveGapEvidence(3, 0, request, response), /request archive gaps/u);
  assert.throws(() => assertArchiveGapEvidence(0, 2, request, response), /response archive gaps/u);
  assert.throws(() => assertArchiveGapEvidence(0, 0, { ...request, failed: 1 }, response), /capture failures/u);
  assert.throws(() => assertArchiveGapEvidence(0, 0, request, { ...response, failed: 1 }), /capture failures/u);
  assert.throws(() => assertArchiveGapEvidence(0, 0, { ...request, retention_limit: 1 }, response), /retention limit/u);
  assert.throws(() => assertArchiveGapEvidence(0, 0, request, { failed: 0 }), /capacity rejection counter/u);
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
  const harness = readFileSync(new URL("./benchmark-durable-archive-memory.ts", import.meta.url), "utf8");
  assert.match(workflow, /node tests\/load\/benchmark-durable-archive-memory\.ts\s+\\\s+target\/release\/memeloop-token-center/u);
  assert.match(workflow, /DURABLE_RSS_EXIT_CODE/u);
  assert.match(harness, /processMemory\(service/u);
  assert.match(harness, /high_water_mib/u);
  assert.match(harness, /native_allocator_bytes/u);
  assert.match(harness, /process_memory/u);
  assert.match(harness, /mock_pid = process\.pid/u);
  assert.doesNotMatch(harness, /cargo\s+build/u);
});

test("production and durable acceptance fix the same native mmap threshold", () => {
  assert.equal(NATIVE_MMAP_THRESHOLD_BYTES, 64 * 1024);
  assert.deepEqual(nativeAllocatorEnvironment(), {
    GLIBC_TUNABLES: "glibc.malloc.mmap_threshold=65536",
  });
  const dockerfile = readFileSync(new URL("../../Dockerfile", import.meta.url), "utf8");
  assert.match(dockerfile, /GLIBC_TUNABLES=glibc\.malloc\.mmap_threshold=65536/u);
});
