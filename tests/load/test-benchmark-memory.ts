#!/usr/bin/env node

import assert from "node:assert/strict";
import test from "node:test";
import { assetGatewayRssEvidence, chatPayload, createMockServer, MockState, seed } from "./benchmark-memory.ts";

test("asset gateway gate uses elevated phase start, not original idle", () => {
  const evidence = assetGatewayRssEvidence(180, 150, 40); assert.equal(evidence.gateway_phase_delta_rss_mib, 30); assert.equal(evidence.gateway_cumulative_delta_from_original_idle_mib, 140); assert.ok(evidence.gateway_phase_delta_rss_mib <= 96); assert.ok(evidence.gateway_cumulative_delta_from_original_idle_mib > 96);
});
test("asset gateway phase delta never reports negative growth", () => assert.equal(assetGatewayRssEvidence(149, 150, 40).gateway_phase_delta_rss_mib, 0));

test("every benchmark route explicitly confirms its custom model", async () => {
  const requests: Array<[string, Record<string, any>]> = [];
  const requestJson = async (_url: string, _method: string, path: string, _token: string, payload?: unknown): Promise<Record<string, any>> => { requests.push([path, payload as Record<string, any>]); if (path === "/internal/v1/upstreams") return { id: `upstream-${requests.length}` }; if (path === "/internal/v1/model-routes") return { id: `route-${requests.length}` }; if (path === "/internal/v1/keys") return { key: "mts_test" }; return {}; };
  const key = await seed("http://control.invalid", "http://gateway.invalid", "service-token", "http://mock.invalid", requestJson, async () => 1); assert.equal(key, "mts_test"); const routes = requests.filter(([path]) => path === "/internal/v1/model-routes").map(([, payload]) => payload); assert.equal(routes.length, 3); assert.deepEqual(new Set(routes.map((route) => route.protocol)), new Set(["openai", "generation"])); const keyPayload = requests.find(([path]) => path === "/internal/v1/keys")![1]; assert.equal(keyPayload.route_ids.length, 3); assert.ok(!("allowed_models" in keyPayload.policy)); for (const route of routes) { assert.equal(route.custom_model_confirmed, true); assert.equal(route.priority, 0); }
});

test("stream fixture exercises the streaming proxy path", async () => {
  const server = createMockServer(new MockState()); await new Promise<void>((done) => server.listen(0, "127.0.0.1", done)); try { const address = server.address(); assert.ok(address && typeof address !== "string"); const response = await fetch(`http://127.0.0.1:${address.port}/v1/chat/completions`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(chatPayload("stream", 4096)) }); assert.equal(response.status, 200); assert.equal(response.headers.get("content-type"), "text/event-stream"); assert.equal((await response.arrayBuffer()).byteLength, 4096); } finally { await new Promise<void>((done) => server.close(() => done())); }
});
