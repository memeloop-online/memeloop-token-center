import assert from "node:assert/strict";
import { realpathSync } from "node:fs";
import http from "node:http";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

export const CHECKPOINT = "synthetic-opaque-checkpoint";
export const HISTORY = [
  { type: "message", role: "user", content: [{ type: "input_text", text: "Inspect this synthetic workspace." }] },
  { type: "function_call", call_id: "call_synthetic", name: "inspect", arguments: "{}" },
  { type: "function_call_output", call_id: "call_synthetic", output: "Synthetic inspection completed." },
  { type: "message", role: "user", content: [{ type: "input_text", text: "Continue." }] },
];
const MODEL = "synthetic-upstream-model";
const NORMAL_PATH = "/v1/responses";
const COMPACT_PATH = "/v1/responses/compact";
type Json = Record<string, any>;
type Attempt = { path: string; body: Json };
type Phase = { name: string; driver: string; compact: boolean; stream: boolean };
export const PHASES: Phase[] = [
  { name: "http_json_responses", driver: "http-json", compact: false, stream: true },
  { name: "http_json_compact_json", driver: "http-json", compact: true, stream: false },
  { name: "http_json_compact_sse", driver: "http-json", compact: true, stream: true },
  { name: "new_api_responses", driver: "new-api", compact: false, stream: true },
  { name: "new_api_compact_json", driver: "new-api", compact: true, stream: false },
  { name: "new_api_compact_sse", driver: "new-api", compact: true, stream: true },
];

export function configForPhase(phase: Phase): Json {
  return {
    base_url: "http://127.0.0.1:18080",
    timeout_seconds: 15,
    ...(phase.driver === "http-json" && phase.compact ? { responses_compact_v2_bridge: true } : {}),
  };
}

export function verifyCompactEvidence(text: string) {
  const records = text.split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line) as Json);
  assert.equal(records.length, PHASES.length + 1, "require every phase and terminal evidence");
  PHASES.forEach((phase, index) => {
    assert.deepEqual(records[index], {
      phase: phase.name, status: 200, posts: 1,
      upstream_path: phase.compact ? COMPACT_PATH : NORMAL_PATH,
      downstream_status: "completed", checkpoint_preserved: phase.compact,
    });
  });
  assert.deepEqual(records.at(-1), { compact_checks: "passed", phases: PHASES.length, posts: PHASES.length });
}

export function inputForPhase(phase: Phase): Json {
  return {
    model: phase.name, stream: phase.stream, instructions: "Preserve the synthetic conversation.",
    input: phase.compact ? [...HISTORY, { type: "compaction_trigger" }] : HISTORY,
    tools: [{ type: "function", name: "inspect", parameters: { type: "object", properties: {} } }],
    reasoning: { effort: "high" }, text: { format: { type: "text" } },
  };
}

export function validateAttempt(phase: Phase, attempts: Attempt[]) {
  assert.equal(attempts.length, 1, "each client request must dispatch exactly one POST");
  const attempt = attempts[0];
  assert.ok(attempt);
  assert.equal(attempt.path, phase.compact ? COMPACT_PATH : NORMAL_PATH);
  assert.equal(attempt.body.model, MODEL);
  assert.equal(attempt.body.stream, phase.compact ? false : phase.stream);
  assert.deepEqual(attempt.body.input, HISTORY);
  assert.equal(attempt.body.instructions, "Preserve the synthetic conversation.");
  if (phase.compact) {
    for (const key of ["tools", "reasoning", "text", "max_output_tokens"]) {
      assert.equal(Object.hasOwn(attempt.body, key), false, `compact must remove ${key}`);
    }
  }
}

export function normalResponse(): Json {
  return {
    id: "resp_synthetic", object: "response", status: "completed", error: null,
    model: MODEL, output: [{
      id: "msg_synthetic", type: "message", role: "assistant", status: "completed",
      content: [{ type: "output_text", text: "pong", annotations: [] }],
    }], usage: { input_tokens: 9, output_tokens: 2, total_tokens: 11 },
  };
}

export function toSse(response: Json): string {
  const item = response.output[0];
  return [
    { type: "response.created", response: { ...response, status: "in_progress", output: [] } },
    { type: "response.output_item.added", output_index: 0, item },
    { type: "response.output_item.done", output_index: 0, item },
    { type: "response.completed", response },
  ].map(event => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join("");
}

export function validateDownstream(phase: Phase, contentType: string, text: string) {
  let response: Json;
  if (phase.stream) {
    assert.ok(contentType.startsWith("text/event-stream"));
    const events = text.split(/\r?\n\r?\n/).flatMap(block => {
      const data = block.split(/\r?\n/).filter(line => line.startsWith("data:"))
        .map(line => line.slice(5).trim()).join("\n");
      return !data || data === "[DONE]" ? [] : [JSON.parse(data) as Json];
    });
    assert.equal(events.filter(event => event.type === "response.completed").length, 1);
    assert.equal(events.filter(event => event.type === "response.output_item.done").length, 1);
    assert.ok(!events.some(event => event.type === "error" || event.type === "response.failed"));
    if (phase.compact) {
      assert.deepEqual(events.map(event => event.type), [
        "response.created", "response.output_item.added",
        "response.output_item.done", "response.completed",
      ]);
    }
    response = events.find(event => event.type === "response.completed")!.response;
    assert.deepEqual(
      events.find(event => event.type === "response.output_item.done")!.item,
      response.output[0],
      "completed response must contain the same finished output item",
    );
  } else {
    assert.ok(contentType.startsWith("application/json"));
    response = JSON.parse(text);
  }
  assert.equal(response.object, "response");
  assert.equal(response.status, "completed");
  assert.equal(response.error, null);
  assert.equal(response.output.length, 1);
  assert.equal(response.usage.input_tokens, 9);
  assert.equal(response.usage.output_tokens, 2);
  if (phase.compact) {
    assert.equal(response.output[0].type, "compaction");
    assert.equal(response.output[0].encrypted_content, CHECKPOINT);
  } else {
    assert.equal(response.output[0].type, "message");
    assert.equal(response.output[0].content[0].text, "pong");
  }
}

let stage = "bootstrap";
async function run() {
  const token = process.env.MTC_PROBE_SERVICE_TOKEN;
  assert.ok(token && token.length >= 32);
  const attempts: Attempt[] = [];
  let mockFailure = false;
  const mock = http.createServer(async (request, response) => {
    try {
      response.setHeader("content-type", "application/json");
      if (request.method !== "POST") {
        request.resume();
        response.end(JSON.stringify({
          object: "list", data: [{ id: MODEL, object: "model", owned_by: "synthetic" }],
        }));
        return;
      }
      const chunks: Buffer[] = [];
      let size = 0;
      for await (const chunk of request) {
        const bytes = Buffer.from(chunk);
        size += bytes.length;
        assert.ok(size <= 64 * 1024, "unexpectedly large synthetic request");
        chunks.push(bytes);
      }
      const body = JSON.parse(Buffer.concat(chunks).toString()) as Json;
      const path = request.url ?? "";
      attempts.push({ path, body });
      if (path === COMPACT_PATH) {
        response.end(JSON.stringify({
          id: "cmp_synthetic", output: [{ id: "checkpoint", type: "compaction", encrypted_content: CHECKPOINT }],
          usage: { input_tokens: 9, output_tokens: 2, total_tokens: 11 },
        }));
      } else if (path === NORMAL_PATH) {
        response.setHeader("content-type", "text/event-stream");
        response.end(toSse(normalResponse()));
      } else {
        response.statusCode = 404;
        response.end("{}");
      }
    } catch {
      mockFailure = true;
      response.statusCode = 500;
      response.end("{}");
    }
  });
  await new Promise<void>(resolve => mock.listen(18080, "127.0.0.1", resolve));
  const base = "http://127.0.0.1:8080";
  async function control(path: string, body: Json): Promise<Json> {
    const result = await fetch(base + path, {
      method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify(body), signal: AbortSignal.timeout(15_000),
    });
    assert.ok(result.ok);
    return await result.json() as Json;
  }
  try {
    stage = "readiness";
    let ready = false;
    for (let index = 0; index < 60; index++) {
      ready = await fetch(base + "/readyz", { signal: AbortSignal.timeout(1_000) })
        .then(response => response.ok).catch(() => false);
      if (ready) break;
      await sleep(1_000);
    }
    assert.ok(ready);
    for (const phase of PHASES) {
      stage = `configure_${phase.name}`;
      const account = await control("/internal/v1/upstreams", {
        name: phase.name, driver: phase.driver,
        config: configForPhase(phase),
        credential: { type: "api_key", value: "MTC_COMPACT_SYNTHETIC_CREDENTIAL" },
      });
      const route = await control("/internal/v1/model-routes", {
        public_model: phase.name, upstream_account_id: account.id,
        upstream_model: MODEL, protocol: "openai", custom_model_confirmed: true,
      });
      await control(`/internal/v1/prices/USD/${phase.name}`, {
        input_per_million: "0", output_per_million: "0",
      });
      const key = await control("/internal/v1/keys", {
        principal_external_id: "synthetic-compact-probe", alias: phase.name,
        route_ids: [route.id], policy: { enforcement_mode: "metered_unlimited" },
      });
      stage = `request_${phase.name}`;
      const before = attempts.length;
      const result = await fetch(base + NORMAL_PATH, {
        method: "POST", headers: {
          authorization: `Bearer ${key.key}`, "content-type": "application/json",
          accept: phase.stream ? "text/event-stream" : "application/json",
        }, body: JSON.stringify(inputForPhase(phase)), signal: AbortSignal.timeout(45_000),
      });
      const text = await result.text();
      assert.equal(result.status, 200);
      validateAttempt(phase, attempts.slice(before));
      validateDownstream(phase, result.headers.get("content-type") ?? "", text);
      assert.equal(mockFailure, false);
      console.log(JSON.stringify({
        phase: phase.name, status: result.status, posts: attempts.length - before,
        upstream_path: phase.compact ? COMPACT_PATH : NORMAL_PATH,
        downstream_status: "completed", checkpoint_preserved: phase.compact,
      }));
    }
    assert.equal(attempts.length, PHASES.length);
    console.log(JSON.stringify({ compact_checks: "passed", phases: PHASES.length, posts: attempts.length }));
  } finally {
    mock.close();
    mock.closeAllConnections();
  }
}

if (process.argv[1] && realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url))) {
  run().catch(() => {
    console.error(JSON.stringify({ error: "synthetic compact runtime probe failed", stage }));
    process.exitCode = 1;
  });
}
