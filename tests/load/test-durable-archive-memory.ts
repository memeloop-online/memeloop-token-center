import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { allocatorEvidence, archiveDrainReady, assertArchiveGapEvidence, assertArchiveReplayGapEvidence, assertUnknownArchiveEvidence, assertUnknownFinancialEvidence, assertUnknownSemanticEvidence, deferredPersistenceEvidence, requestPersistenceEvidence, inputPlan, memoryVerdict, nativeAllocatorEnvironment, nativeAllocatorEvidence, NATIVE_MMAP_THRESHOLD_BYTES, outputPlan, permitEvidence, planHash, requestPath, responsesInputPlan, responsesOutputPlan } from "./benchmark-durable-archive-memory.ts";
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
  const retained = { pending_requests: 1, unsettled_reservations: 1, cipher_bytes: 0, request_cipher_bytes: 0 };
  assert.equal(archiveDrainReady(retained), false);
  assert.equal(archiveDrainReady(retained, 0), true);
  assert.equal(archiveDrainReady(empty, 0), false);
  for (const field of Object.keys(retained)) {
    assert.equal(archiveDrainReady({ ...retained, [field]: retained[field as keyof typeof retained] + 1 }, 0), false);
  }
  assert.equal(archiveDrainReady({ ...retained, pending_requests: 0 }, 0), false);
  assert.equal(archiveDrainReady({ ...retained, unsettled_reservations: 0 }, 0), false);
  assert.throws(() => archiveDrainReady(retained, NaN), /retained archive bytes/u);
  assert.throws(() => archiveDrainReady(retained, -1), /retained archive bytes/u);
});

test("unknown requires owned semantics exactly once while retaining its receipt, hold and no financial ACK", () => {
  const responseObject = 'gap://request/response';
  const claim = {
    request_id: "request", tenant_id: "tenant", key_id: "key", account_id: "account",
    reservation_id: "reservation", owner_token: "owner", fence: "fence", contract_digest: "digest",
  };
  const row = {
    ...claim, financial_completed: null, financial_status: null, actual_micros: null,
    receipt_id: null, receipt_digest: null, cost_micros: null, reserved_micros: 123,
    reservation_status: "reserved", state: "pending", unknown_reason: "authoritative_usage_unavailable",
    ledger_count: 0, event_count: 1, fact_count: 1, observation_count: 1, projected_event_count: 1,
    stats_usage_basis: "not_observed", stats_cost_micros: 0, stats_input_tokens: 0,
    stats_output_tokens: 0, stats_cached_input_tokens: 0, stats_cache_write_tokens: 0,
    stats_status_class: "failure", stats_error_code: "upstream_invalid_response",
    stats_tenant_id: "tenant", stats_key_id: "key", finished_cost_micros: 0,
    finished_tenant_id: "tenant", finished_key_id: "key",
    principal_id: "principal", request_cluster_id: "cluster",
    observation_id: "observation", observation_request_id: "request", observation_key_id: "key",
    observation_cluster_id: "cluster", cluster_tenant_id: "tenant", cluster_principal_id: "principal",
    projection_tenant_id: "tenant", projection_key_id: "key", projection_principal_id: "principal",
    projection_completed: 1235, projection_observed_at: 1234, projected_event_id: "projected",
    projected_event_tenant_id: "tenant", projected_event_key_id: "key",
    operational_status: 502, operational_error: "upstream_invalid_response",
    operational_stage: "buffered_response_ready", operational_completed: 1234,
    operational_receipt_id: "receipt", operational_response_object: responseObject,
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
  assert.equal(assertUnknownFinancialEvidence(row, receipt), 0);
  const semantics = assertUnknownSemanticEvidence(row);
  assert.deepEqual(assertUnknownSemanticEvidence({ ...row }, semantics), semantics);
  assert.equal(assertUnknownFinancialEvidence({ ...row }, receipt), 0);
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
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, ledger_count: 1 }, receipt), /financial side effects/u);
  for (const field of ["event_count", "fact_count"]) {
    for (const count of [0, 2]) assert.throws(() => assertUnknownFinancialEvidence({ ...row, [field]: count }, receipt), /exactly one observed stats/u);
  }
  for (const field of ["stats_cost_micros", "stats_input_tokens", "stats_output_tokens", "stats_cached_input_tokens", "stats_cache_write_tokens", "finished_cost_micros"]) {
    assert.throws(() => assertUnknownFinancialEvidence({ ...row, [field]: 1 }, receipt), /fabricate billing or usage/u);
  }
  for (const field of ["stats_tenant_id", "stats_key_id", "finished_tenant_id", "finished_key_id"]) {
    assert.throws(() => assertUnknownFinancialEvidence({ ...row, [field]: "other" }, receipt), /exact ownership/u);
  }
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, stats_usage_basis: "provider_reported" }, receipt), /unknown provider usage/u);
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, stats_status_class: "success" }, receipt), /real upstream failure/u);
  assert.throws(() => assertUnknownFinancialEvidence({ ...row, stats_error_code: "" }, receipt), /real upstream failure/u);
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
  assert.throws(() => assertUnknownFinancialEvidence(row, { ...receipt, response_object: "inline-json:private upstream body" }), /raw response handoff/u);
});

test("unknown source requires exact tenant CAS roots, source digests and observed original bytes", () => {
  const archive = mkdtempSync(join(tmpdir(), "mtc-unknown-source-contract-"));
  const requestId = "00000000-0000-4000-8000-000000000001";
  const tenant = "00000000-0000-4000-8000-000000000002";
  const digest = "a".repeat(64);
  const locator = `tenants/${tenant}/cas/v1/blake3/aa/${digest}`;
  const staged = `staging/proxy/${requestId}/request/${requestId}/body.json`;
  const row = {
    request_id: requestId, tenant_id: tenant, request_object: locator, bound_locator: staged, spool_state: "bound",
    body_byte_count: 512, body_blake3: digest, source_intent_digest: digest,
    spool_plain_bytes: 0, spool_cipher_bytes: 0, chunk_count: 0,
  };
  const plan = inputPlan(512);
  const body = Buffer.concat([plan.prefix, Buffer.alloc(plan.fillBytes, "x"), plan.suffix]);
  const writeOriginal = (side: string, hash: string, bytes: Buffer) => {
    const object = `tenants/${tenant}/cas/v1/blake3/${hash.slice(0, 2)}/${hash}`;
    const root = join(archive, `tenants/${tenant}/raw/v1/${requestId}/${side}.json`);
    mkdirSync(dirname(root), { recursive: true });
    mkdirSync(dirname(join(archive, object)), { recursive: true });
    writeFileSync(join(archive, object), bytes);
    writeFileSync(root, JSON.stringify({ object_locator: object, size_bytes: bytes.length, blake3_digest: hash }));
  };
  try {
    writeOriginal("request", digest, body);
    writeOriginal("response", "b".repeat(64), outputPlan(512).prefix.subarray(0, 16));
    const evidence = assertUnknownArchiveEvidence(row, archive);
    assert.equal(evidence.locator, locator);
    assert.equal(evidence.sha256, planHash(plan));
    assert.equal(evidence.complete_training_fidelity_certified, false);
    assert.throws(() => assertUnknownArchiveEvidence({ ...row, request_id: "../outside" }, archive), /canonical request identity/u);
    assert.throws(() => assertUnknownArchiveEvidence({ ...row, tenant_id: "../outside" }, archive), /canonical tenant identity/u);
    assert.throws(() => assertUnknownArchiveEvidence({ ...row, source_intent_digest: "b".repeat(64) }, archive), /staging digest/u);
    assert.throws(() => assertUnknownArchiveEvidence({ ...row, bound_locator: "../outside" }, archive), /exact request-bound source locator/u);
    assert.throws(() => assertUnknownArchiveEvidence({ ...row, request_object: staged }, archive), /exact CAS locator/u);
    for (const state of ["pending", "capturing", "gap"]) assert.throws(() => assertUnknownArchiveEvidence({ ...row, spool_state: state }, archive), /remain durable/u);
    assert.throws(() => assertUnknownArchiveEvidence({ ...row, body_byte_count: 511 }, archive), /exact 512 byte length/u);
    for (const field of ["spool_plain_bytes", "spool_cipher_bytes", "chunk_count"]) {
      assert.throws(() => assertUnknownArchiveEvidence({ ...row, [field]: 1 }, archive), /zero encrypted spool accounting/u);
    }
    writeOriginal("response", "b".repeat(64), Buffer.from("safe local error"));
    assert.throws(() => assertUnknownArchiveEvidence(row, archive), /original response bytes/u);
    writeOriginal("response", "b".repeat(64), outputPlan(512).prefix.subarray(0, 16));
    writeFileSync(join(archive, locator), Buffer.alloc(512, "y"));
    assert.throws(() => assertUnknownArchiveEvidence(row, archive), /original request bytes/u);
    rmSync(join(archive, locator));
    assert.throws(() => assertUnknownArchiveEvidence(row, archive), /ENOENT/u);
  } finally { rmSync(archive, { recursive: true, force: true }); }
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

test("journal replay preserves accounted gaps across process counter reset without accepting new gaps or failures", () => {
  const baseline = {
    successful_request_archive_gaps: 4, successful_response_archive_gaps: 2,
    request_persistence: { accepted: 3, capacity: 4, failed: 0, retention_limit: 0 },
    persistence: { accepted: 4, capacity: 2, failed: 0 },
  };
  const request = { accepted: 0, capacity: 0, failed: 0, retention_limit: 0 };
  const response = { accepted: 0, capacity: 0, failed: 0 };
  assertArchiveReplayGapEvidence(4, 2, request, response, baseline);
  assert.throws(() => assertArchiveGapEvidence(4, 2, request, response), /request archive gaps/u);
  for (const gaps of [3, 5]) {
    assert.throws(() => assertArchiveReplayGapEvidence(gaps, 2, { ...request, capacity: 10 }, response, baseline), /preserve.*request archive gaps/u);
  }
  for (const gaps of [1, 3]) {
    assert.throws(() => assertArchiveReplayGapEvidence(4, gaps, request, { ...response, capacity: 10 }, baseline), /preserve.*response archive gaps/u);
  }
  assert.throws(() => assertArchiveReplayGapEvidence(4, 2, request, response, { ...baseline, request_persistence: { ...baseline.request_persistence, capacity: 3 } }), /request archive gaps/u);
  assert.throws(() => assertArchiveReplayGapEvidence(4, 2, request, response, { ...baseline, persistence: { ...baseline.persistence, capacity: 1 } }), /response archive gaps/u);
  assert.throws(() => assertArchiveReplayGapEvidence(4, 2, { ...request, failed: 1 }, response, baseline), /capture failures/u);
  assert.throws(() => assertArchiveReplayGapEvidence(4, 2, request, { ...response, failed: 1 }, baseline), /capture failures/u);
  assert.throws(() => assertArchiveReplayGapEvidence(4, 2, { ...request, retention_limit: 1 }, response, baseline), /retention limit/u);
  assert.throws(() => assertArchiveReplayGapEvidence(4, 2, { ...request, capacity: NaN }, response, baseline), /capacity rejection counter/u);
  assert.throws(() => assertArchiveReplayGapEvidence(4, 2, request, { failed: 0 }, baseline), /capacity rejection counter/u);
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
