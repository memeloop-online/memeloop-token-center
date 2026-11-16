import { test } from "node:test";
import assert from "node:assert/strict";
import { validateBaseline, baselineIdentity } from "./baseline.ts";
import { createStagingMock } from "./mock.ts";

test("missing natural peak evidence cannot become a passing baseline", () => {
  for (const value of [null, {}, { schema_version: 1, windows: [] }]) assert.throws(() => validateBaseline(value));
  const measured = { schema_version: 1 as const, evidence: { source: "synthetic-unit-contract-only", observed_from: "2026-01-01T00:00:00Z", observed_until: "2026-01-01T01:00:00Z", query_sha256: "a".repeat(64) }, windows: [{ seconds: 60, requests: 120, max_in_flight: 4, input_bytes: 1000, output_bytes: 2000 }], protocol_requests: { chat: 120 } };
  assert.equal(baselineIdentity(measured).length, 64);
  assert.throws(() => validateBaseline({ ...measured, windows: [{ ...measured.windows[0], requests: 0 }] }));
});

test("external staging mock reuses gateway JSON/SSE contracts and isolates failures", async () => {
  const { server } = createStagingMock();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address(); assert(address && typeof address !== "string");
  const url = `http://127.0.0.1:${address.port}`;
  try {
    const json = await fetch(`${url}/scenario/normal/v1/chat/completions`, { method: "POST", body: JSON.stringify({ model: "isolated-mock", messages: [{ role: "user", content: "test" }] }) });
    assert.equal(json.status, 200); assert.equal((await json.json() as any).choices[0].message.content, "ok");
    const sse = await fetch(`${url}/scenario/normal/v1/chat/completions`, { method: "POST", body: JSON.stringify({ model: "isolated-mock", benchmark: { mode: "stream", bytes: 8192, chunk_bytes: 4096 } }) });
    assert.equal(sse.status, 200); assert.match(await sse.text(), /data: \[DONE\]/);
    const failure = await fetch(`${url}/scenario/upstream-error/v1/chat/completions`, { method: "POST", body: "{}" });
    assert.equal(failure.status, 503); await failure.arrayBuffer();
    const unknown = await fetch(`${url}/v1/chat/completions`, { method: "POST", body: "{}" });
    assert.equal(unknown.status, 404);
  } finally { server.closeAllConnections(); await new Promise<void>((resolve) => server.close(() => resolve())); }
});
