#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import test from "node:test";
import { assetGatewayRssEvidence, chatPayload, createMockServer, HarnessFailure, MockState, seed, smallChat, streamChat, waitForTextRouteRecovery } from "../../ops/benchmark-memory.ts";
import { StreamStartBarrier } from "../../ops/benchmark-stream-barrier.ts";

const benchmarkEntry = resolve(import.meta.dirname, "../../ops/benchmark-memory.ts");

test("TypeScript CLI exposes the memory harness without a shell wrapper", () => {
  const result = spawnSync(process.execPath, [benchmarkEntry, "--help"], { encoding: "utf8", shell: false });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /^Usage: benchmark-memory\.ts /u);
});

test("TypeScript CLI preserves prerequisite exit code and report evidence", () => {
  const temporary = mkdtempSync(resolve(tmpdir(), "mtc-memory-cli-test-"));
  try {
    const output = resolve(temporary, "report.json");
    const missingBinary = resolve(temporary, "missing-binary");
    const result = spawnSync(process.execPath, [benchmarkEntry, "--profile", "short", "--binary", missingBinary, "--output", output], { encoding: "utf8", shell: false });
    assert.equal(result.status, 3);
    const report = JSON.parse(readFileSync(output, "utf8")) as Record<string, unknown>;
    assert.equal(report.exit_code, 3);
    assert.equal(report.error_kind, "prerequisite");
    assert.equal(report.binary, missingBinary);
    assert.equal(report.passed, false);
  } finally {
    rmSync(temporary, { force: true, recursive: true });
  }
});

test("asset gateway gate uses elevated phase start, not original idle", () => {
  const evidence = assetGatewayRssEvidence(180, 150, 40); assert.equal(evidence.gateway_phase_delta_rss_mib, 30); assert.equal(evidence.gateway_cumulative_delta_from_original_idle_mib, 140); assert.ok(evidence.gateway_phase_delta_rss_mib <= 96); assert.ok(evidence.gateway_cumulative_delta_from_original_idle_mib > 96);
});
test("asset gateway phase delta never reports negative growth", () => assert.equal(assetGatewayRssEvidence(149, 150, 40).gateway_phase_delta_rss_mib, 0));

test("stream start barrier releases only after every configured stream arrives", async () => {
  const barrier = new StreamStartBarrier(3, 1_000);
  let released = false;
  const first = barrier.arrive().then((value) => { released = value; return value; });
  await Promise.resolve();
  assert.equal(released, false);
  const second = barrier.arrive();
  await Promise.resolve();
  assert.equal(released, false);
  const third = barrier.arrive();
  assert.deepEqual(await Promise.all([first, second, third]), [true, true, true]);
  assert.deepEqual(barrier.evidence(), { required: 3, admitted: 3, released: true, failure: null });
});

test("stream start barrier releases every waiter after a client disconnect", async () => {
  const barrier = new StreamStartBarrier(3, 1_000);
  const controller = new AbortController();
  const first = barrier.arrive();
  const disconnected = barrier.arrive(controller.signal);
  controller.abort();
  assert.deepEqual(await Promise.all([first, disconnected]), [false, false]);
  assert.deepEqual(barrier.evidence(), { required: 3, admitted: 2, released: true, failure: "client_disconnected" });
});

test("stream start barrier releases waiters after its bounded timeout", async () => {
  const barrier = new StreamStartBarrier(2, 1);
  assert.equal(await barrier.arrive(), false);
  assert.deepEqual(barrier.evidence(), { required: 2, admitted: 1, released: true, failure: "timeout" });
});

test("every benchmark route explicitly confirms its custom model", async () => {
  const requests: Array<[string, Record<string, any>]> = [];
  const requestJson = async (_url: string, _method: string, path: string, _token: string, payload?: unknown): Promise<Record<string, any>> => { requests.push([path, payload as Record<string, any>]); if (path === "/internal/v1/upstreams") return { id: `upstream-${requests.length}` }; if (path === "/internal/v1/model-routes") return { id: `route-${requests.length}` }; if (path === "/internal/v1/keys") return { key: "mts_test" }; return {}; };
  const key = await seed("http://control.invalid", "http://gateway.invalid", "service-token", "http://mock.invalid", requestJson, async () => 1); assert.equal(key, "mts_test"); const routes = requests.filter(([path]) => path === "/internal/v1/model-routes").map(([, payload]) => payload); assert.equal(routes.length, 4); assert.deepEqual(new Set(routes.map((route) => route.protocol)), new Set(["openai", "generation"])); const keyPayload = requests.find(([path]) => path === "/internal/v1/keys")![1]; assert.equal(keyPayload.route_ids.length, 4); assert.equal(keyPayload.policy.enforcement_mode, "metered_unlimited"); assert.ok(!("allowed_models" in keyPayload.policy)); for (const route of routes) { assert.equal(route.custom_model_confirmed, true); assert.equal(route.priority, 0); } const responsesUpstream = requests.filter(([path]) => path === "/internal/v1/upstreams").map(([, payload]) => payload).find((payload) => payload.config?.image_api_mode === "responses-tool"); assert.equal(responsesUpstream?.config.image_main_model, "gpt-5.6-sol");
});

test("stream fixture exercises the streaming proxy path with valid Chat SSE", async () => {
  const server = createMockServer(new MockState()); await new Promise<void>((done) => server.listen(0, "127.0.0.1", done)); try { const address = server.address(); assert.ok(address && typeof address !== "string"); const response = await fetch(`http://127.0.0.1:${address.port}/v1/chat/completions`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(chatPayload("stream", 4096)) }); assert.equal(response.status, 200); assert.equal(response.headers.get("content-type"), "text/event-stream"); const body = Buffer.from(await response.arrayBuffer()); assert.equal(body.length, 4096); const text = body.toString("utf8"); assert.match(text, /"id":"chatcmpl-memory-stream"/u); assert.match(text, /"finish_reason":"stop"/u); assert.match(text, /"prompt_tokens":2,"completion_tokens":1,"total_tokens":3/u); assert.ok(text.endsWith("data: [DONE]\n\n")); } finally { await new Promise<void>((done) => server.close(() => done())); }
});

test("soak chat uses bounded node:http requests instead of Undici fetch", async () => {
  const server = createMockServer(new MockState()); await new Promise<void>((done) => server.listen(0, "127.0.0.1", done)); const originalFetch = globalThis.fetch; globalThis.fetch = () => { throw new Error("Undici fetch must not be used by soak chat"); }; try { const address = server.address(); assert.ok(address && typeof address !== "string"); for (let index = 0; index < 25; index += 1) assert.ok(await smallChat(`http://127.0.0.1:${address.port}`, "test-key") > 0); } finally { globalThis.fetch = originalFetch; await new Promise<void>((done) => server.close(() => done())); }
});

test("soak waits for the intentional response-limit breaker to recover", async () => {
  let attempts = 0;
  const evidence = await waitForTextRouteRecovery(async () => {
    attempts += 1;
    if (attempts < 3) throw new HarnessFailure('small chat failed with HTTP 503: {"error":{"message":"no healthy upstream is currently available","type":"upstream_error"}}');
    return 1;
  }, 1_000, 0);
  assert.equal(evidence.attempts, 3);
  assert.equal(evidence.temporarily_unavailable, 2);
  assert.ok(evidence.duration_seconds >= 0);
});

test("soak recovery fails closed for an unrelated upstream error", async () => {
  await assert.rejects(waitForTextRouteRecovery(async () => { throw new HarnessFailure('small chat failed with HTTP 503: {"error":{"message":"maintenance"}}'); }, 1_000, 0), /maintenance/u);
});

test("large stream is counted incrementally without Undici or body aggregation", async () => {
  const server = createMockServer(new MockState()); await new Promise<void>((done) => server.listen(0, "127.0.0.1", done)); const originalFetch = globalThis.fetch; globalThis.fetch = () => { throw new Error("Undici fetch must not be used by stream chat"); }; try { const address = server.address(); assert.ok(address && typeof address !== "string"); assert.equal(await streamChat(`http://127.0.0.1:${address.port}`, "test-key", 4 * 1024 * 1024), 4 * 1024 * 1024); } finally { globalThis.fetch = originalFetch; await new Promise<void>((done) => server.close(() => done())); }
});
