import assert from "node:assert/strict";
import test from "node:test";
import {
  CHECKPOINT, HISTORY, PHASES, configForPhase, configurePhase, inputForPhase, normalResponse, toSse,
  validateAttempt, validateDownstream, verifyCompactEvidence,
} from "./compact-runtime-probe.ts";
import { renderJob } from "./dns-runtime-job.ts";

test("actual phase setup prices the upstream model and retains the legacy alias fixture before admission", async () => {
  for (const phase of PHASES) {
    const calls: { path: string; body: Record<string, any> }[] = [];
    const key = await configurePhase(phase, async (path, body) => {
      calls.push({ path, body });
      if (path === "/internal/v1/upstreams") return { id: "synthetic-account" };
      if (path === "/internal/v1/model-routes") return { id: "synthetic-route" };
      if (path === "/internal/v1/keys") return { key: "synthetic-client-key" };
      return {};
    });
    assert.deepEqual(calls.map(call => call.path), [
      "/internal/v1/upstreams", "/internal/v1/model-routes",
      `/internal/v1/prices/USD/${phase.name}`,
      "/internal/v1/prices/USD/synthetic-upstream-model", "/internal/v1/keys",
    ]);
    const routeCall = calls[1];
    const actualPriceCall = calls[3];
    const keyCall = calls[4];
    assert.ok(routeCall && actualPriceCall && keyCall);
    const route = routeCall.body;
    assert.equal(route.public_model, phase.name);
    assert.equal(route.upstream_account_id, "synthetic-account");
    assert.notEqual(route.upstream_model, route.public_model);
    assert.equal(actualPriceCall.path, `/internal/v1/prices/USD/${route.upstream_model}`);
    for (const call of calls.slice(2, 4)) {
      assert.deepEqual(call.body, { input_per_million: "0", output_per_million: "0" });
    }
    assert.deepEqual(keyCall.body.route_ids, ["synthetic-route"]);
    assert.deepEqual(keyCall.body.policy, { enforcement_mode: "metered_unlimited" });
    assert.equal(key.key, "synthetic-client-key");
  }
});

test("both HTTP-json and New API cover normal, compact JSON, and compact SSE", () => {
  for (const driver of ["http-json", "new-api"]) {
    assert.deepEqual(PHASES.filter(phase => phase.driver === driver)
      .map(phase => [phase.compact, phase.stream]), [[false, true], [true, false], [true, true]]);
  }
  for (const phase of PHASES) {
    const input = inputForPhase(phase);
    assert.equal(input.input.some((item: { type: string }) => item.type === "compaction_trigger"), phase.compact);
    const body = {
      model: "synthetic-upstream-model", stream: phase.compact ? false : phase.stream,
      input: HISTORY, instructions: "Preserve the synthetic conversation.",
    };
    const attempt = { path: phase.compact ? "/v1/responses/compact" : "/v1/responses", body };
    validateAttempt(phase, [attempt]);
    assert.throws(() => validateAttempt(phase, [attempt, attempt]));
    assert.throws(() => validateAttempt(phase, [{ ...attempt, path: "/unexpected" }]));
    if (phase.compact) {
      assert.throws(() => validateAttempt(phase, [{ ...attempt, body: { ...body, input: input.input } }]));
      assert.throws(() => validateAttempt(phase, [{ ...attempt, body: { ...body, tools: [] } }]));
    }
  }
});

test("only HTTP-json compact probes opt in at account scope", () => {
  const actual = PHASES.map(phase => [
    phase.name,
    configForPhase(phase).responses_compact_v2_bridge ?? null,
  ]);
  assert.deepEqual(actual, [
    ["http_json_responses", null],
    ["http_json_compact_json", true],
    ["http_json_compact_sse", true],
    ["new_api_responses", null],
    ["new_api_compact_json", null],
    ["new_api_compact_sse", null],
  ]);
});

test("complete downstream Responses preserve the opaque compact checkpoint", () => {
  for (const phase of PHASES) {
    const response = phase.compact
      ? { ...normalResponse(), output: [{ id: "checkpoint", type: "compaction", encrypted_content: CHECKPOINT }] }
      : normalResponse();
    const text = phase.stream ? toSse(response) : JSON.stringify(response);
    const contentType = phase.stream ? "text/event-stream" : "application/json";
    validateDownstream(phase, contentType, text);
    assert.throws(() => validateDownstream(phase, contentType, text.replaceAll('"completed"', '"failed"')));
    if (phase.stream) {
      assert.throws(() => validateDownstream(phase, contentType, text + text));
      assert.throws(() => validateDownstream(phase, contentType,
        text.split("\n\n").slice(0, -2).join("\n\n")));
    }
    if (phase.compact) {
      assert.throws(() => validateDownstream(phase, contentType, text.replaceAll(CHECKPOINT, "changed")));
    }
  }
});

test("compact evidence requires all six results plus total POST count", () => {
  const results = PHASES.map(phase => ({
    phase: phase.name, status: 200, posts: 1,
    upstream_path: phase.compact ? "/v1/responses/compact" : "/v1/responses",
    downstream_status: "completed", checkpoint_preserved: phase.compact,
  }));
  const lines = [...results, { compact_checks: "passed", phases: 6, posts: 6 }]
    .map(value => JSON.stringify(value));
  verifyCompactEvidence(lines.join("\n"));
  assert.throws(() => verifyCompactEvidence(""));
  assert.throws(() => verifyCompactEvidence(lines.slice(0, -1).join("\n")));
  assert.throws(() => verifyCompactEvidence(lines.reverse().join("\n")));
});

test("compact Job is independent of completed DNS resources and needs no DNS bind capability", () => {
  const digest = "a".repeat(64);
  const image = `ghcr.io/memeloop-online/memeloop-token-center@sha256:${digest}`;
  const node = `node:22@sha256:${digest}`;
  const compact = renderJob(image, node, "compact");
  const dns = renderJob(image, node);
  assert.ok(compact.items.every(item => item.metadata.name === "mtc-compact-runtime-probe"));
  assert.ok(dns.items.every(item => item.metadata.name === "mtc-dns-runtime-probe"));
  const job = compact.items.find(item => item.kind === "Job") as any;
  assert.deepEqual(job.spec.template.spec.containers[0].securityContext.capabilities, { drop: ["ALL"] });
  const script = compact.items.find(item => item.kind === "ConfigMap") as any;
  assert.ok(script.data["probe.ts"].includes("verifyCompactEvidence"));
});
