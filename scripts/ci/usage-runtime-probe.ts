import assert from "node:assert/strict";
import { realpathSync } from "node:fs";
import http from "node:http";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

const MODEL = "gpt-6-astra";
type Json = Record<string, any>;
type Case = "uncapped" | "explicit_cap" | "insufficient_balance";
type Phase = { name: string; driver: string; kind: Case };
export const PHASES: Phase[] = ["http-json", "new-api"].flatMap(driver =>
  (["uncapped", "explicit_cap", "insufficient_balance"] as const)
    .map(kind => ({ name: `${driver}-${kind}`, driver, kind })));

export function accountConfig(): Json {
  return {
    base_url: "http://127.0.0.1:18080", timeout_seconds: 15,
    reservation_token_bounds: { [MODEL]: 65536 },
  };
}

export function inputForPhase(phase: Phase): Json {
  return {
    model: phase.name, input: "Synthetic multi-agent usage probe.", stream: true,
    tools: [{ type: "function", name: "spawn_agent", parameters: { type: "object", properties: {} } }],
    ...(phase.kind === "explicit_cap" ? { max_output_tokens: 4096 } : {}),
  };
}

export function validateForwarded(phase: Phase, attempts: Json[]) {
  assert.equal(attempts.length, phase.kind === "insufficient_balance" ? 0 : 1);
  if (phase.kind === "insufficient_balance") return;
  const attempt = attempts[0];
  assert.ok(attempt);
  assert.equal(attempt.path, "/v1/responses");
  assert.equal(attempt.body.model, MODEL);
  assert.equal(attempt.body.stream, true);
  assert.deepEqual(attempt.body.tools, inputForPhase(phase).tools);
  if (phase.kind === "explicit_cap") {
    assert.equal(attempt.body.max_output_tokens, 4096);
  } else {
    assert.equal(Object.hasOwn(attempt.body, "max_output_tokens"), false,
      "a reservation bound must not become an upstream generation cap");
  }
}

export function validateMetadata(phase: Phase, metadata: Json) {
  if (phase.kind === "uncapped") {
    assert.equal(metadata.status_code, 200);
    assert.equal(metadata.input_tokens, 3);
    assert.equal(metadata.output_tokens, 5000);
    assert.equal(Number(metadata.cost), 0.005003);
  } else if (phase.kind === "explicit_cap") {
    assert.equal(metadata.status_code, 502);
    assert.equal(metadata.error_code, "upstream_invalid_usage");
    assert.equal(metadata.usage_basis, "not_observed");
    assert.equal(metadata.output_tokens, 0);
    assert.equal(Number(metadata.cost), 0);
  }
}

export function verifyUsageEvidence(text: string) {
  const records = text.split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line) as Json);
  assert.equal(records.length, PHASES.length + 1);
  PHASES.forEach((phase, index) => {
    const expected = phase.kind === "uncapped"
      ? { status: 200, input_tokens: 3, output_tokens: 5000, cost: 0.005003, posts: 1 }
      : phase.kind === "explicit_cap"
        ? { status: 502, output_tokens: 0, cost: 0, error_code: "upstream_invalid_usage", posts: 1 }
        : { status: 429, error_code: "balance_exhausted", posts: 0 };
    assert.deepEqual(records[index], { phase: phase.name, ...expected });
  });
  assert.deepEqual(records.at(-1), { usage_checks: "passed", phases: 6, posts: 4 });
}

function responseSse(): string {
  const response = {
    id: "resp_synthetic_usage", object: "response", status: "completed", error: null,
    model: MODEL, output: [{
      id: "msg_synthetic", type: "message", role: "assistant", status: "completed",
      content: [{ type: "output_text", text: "ok", annotations: [] }],
    }],
    usage: {
      input_tokens: 3, input_tokens_details: { cached_tokens: 0 },
      output_tokens: 5000, output_tokens_details: { reasoning_tokens: 0 }, total_tokens: 5003,
    },
  };
  const events = [
    { type: "response.created", response: { ...response, status: "in_progress", output: [] } },
    { type: "response.output_item.done", output_index: 0, item: response.output[0] },
    { type: "response.completed", response },
  ];
  return events.map(event => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join("") + "data: [DONE]\n\n";
}

let stage = "bootstrap";
async function run() {
  const token = process.env.MTC_PROBE_SERVICE_TOKEN;
  assert.ok(token && token.length >= 32);
  const attempts: Json[] = [];
  let mockFailure = false;
  const mock = http.createServer(async (request, response) => {
    try {
      if (request.method !== "POST") {
        request.resume();
        response.setHeader("content-type", "application/json");
        response.end(JSON.stringify({ object: "list", data: [{ id: MODEL, object: "model", owned_by: "synthetic" }] }));
        return;
      }
      const chunks: Buffer[] = [];
      let bytes = 0;
      for await (const chunk of request) {
        const buffer = Buffer.from(chunk);
        bytes += buffer.length;
        assert.ok(bytes <= 64 * 1024);
        chunks.push(buffer);
      }
      attempts.push({ path: request.url, body: JSON.parse(Buffer.concat(chunks).toString()) });
      response.setHeader("content-type", "text/event-stream");
      response.end(responseSse());
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
        name: phase.name, driver: phase.driver, config: accountConfig(),
        credential: { type: "api_key", value: "MTC_USAGE_SYNTHETIC_CREDENTIAL" },
      });
      const route = await control("/internal/v1/model-routes", {
        public_model: phase.name, upstream_account_id: account.id, upstream_model: MODEL,
        protocol: "openai", custom_model_confirmed: true,
      });
      await control(`/internal/v1/prices/USD/${phase.name}`, {
        input_per_million: "1", output_per_million: "1",
      });
      const key = await control("/internal/v1/keys", {
        principal_external_id: phase.name, alias: phase.name, route_ids: [route.id],
        initial_balance: phase.kind === "insufficient_balance" ? "0.01" : "1",
      });
      stage = `request_${phase.name}`;
      const before = attempts.length;
      const result = await fetch(base + "/v1/responses", {
        method: "POST", headers: {
          authorization: `Bearer ${key.key}`, "content-type": "application/json",
          accept: "text/event-stream", "user-agent": "codex_cli_rs/0.155.0",
        }, body: JSON.stringify(inputForPhase(phase)), signal: AbortSignal.timeout(45_000),
      });
      const text = await result.text();
      validateForwarded(phase, attempts.slice(before));
      assert.equal(mockFailure, false);
      if (phase.kind === "insufficient_balance") {
        assert.equal(result.status, 429);
        const body = JSON.parse(text) as Json;
        assert.equal(body.error.reason, "balance_exhausted");
        console.log(JSON.stringify({ phase: phase.name, status: 429, error_code: "balance_exhausted", posts: 0 }));
        continue;
      }
      assert.equal(result.status, 200);
      // Streaming HTTP headers precede settlement; terminal request metadata
      // is authoritative for usage rejection, not the already-sent HTTP 200.
      stage = `settlement_${phase.name}`;
      let metadata: Json | undefined;
      for (let index = 0; index < 100; index++) {
        const response = await fetch(base + "/self/v1/requests", {
          headers: { authorization: `Bearer ${key.key}` }, signal: AbortSignal.timeout(2_000),
        });
        assert.ok(response.ok);
        const records = await response.json() as Json[];
        assert.ok(Array.isArray(records) && records.length <= 1);
        const record = records[0];
        if (record && (phase.kind === "uncapped"
          ? record.status_code === 200 && record.output_tokens === 5000
          : record.status_code === 502 && record.error_code === "upstream_invalid_usage")) {
          metadata = record;
          break;
        }
        await sleep(100);
      }
      assert.ok(metadata, "request did not settle to the expected terminal metadata");
      validateMetadata(phase, metadata);
      if (phase.kind === "uncapped") {
        const events = text.split(/\r?\n/).filter(line => line.startsWith("data:"))
          .map(line => line.slice(5).trim()).filter(value => value !== "[DONE]")
          .map(value => JSON.parse(value) as Json);
        const completed = events.filter(event => event.type === "response.completed");
        assert.equal(completed.length, 1);
        assert.equal(completed[0]!.response.usage.output_tokens, 5000);
        console.log(JSON.stringify({
          phase: phase.name, status: 200, input_tokens: metadata.input_tokens,
          output_tokens: metadata.output_tokens, cost: Number(metadata.cost), posts: 1,
        }));
      } else {
        console.log(JSON.stringify({
          phase: phase.name, status: metadata.status_code, output_tokens: metadata.output_tokens,
          cost: Number(metadata.cost), error_code: metadata.error_code, posts: 1,
        }));
      }
    }
    assert.equal(attempts.length, 4);
    console.log(JSON.stringify({ usage_checks: "passed", phases: PHASES.length, posts: attempts.length }));
  } finally {
    mock.close();
    mock.closeAllConnections();
  }
}

if (process.argv[1] && realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url))) {
  run().catch(() => {
    console.error(JSON.stringify({ error: "synthetic usage runtime probe failed", stage }));
    process.exitCode = 1;
  });
}
