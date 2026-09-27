import assert from "node:assert/strict";
import dgram from "node:dgram";
import http from "node:http";
import net from "node:net";
import { realpathSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

export const HOST = "mtc-dns-probe.localhost";
export const CANARY = "MTC_SYNTHETIC_DNS_SECRET_9d487";
type Mode = "healthy" | "drop" | "nxdomain";
let stage = "bootstrap";

// ConfigMap projected files and Node's import URL can name different symlink
// paths to the same script. Compare canonical paths without executing the probe.
export function isMain(moduleUrl: string, entry: string | undefined): boolean {
  return Boolean(entry && realpathSync(entry) === realpathSync(fileURLToPath(moduleUrl)));
}

function isProbeQuery(query: Buffer): boolean {
  const name = Buffer.concat(HOST.split(".").map(label =>
    Buffer.concat([Buffer.from([label.length]), Buffer.from(label)])));
  return query.subarray(12, 12 + name.length).equals(name);
}

// Deliberately minimal authoritative DNS for one synthetic name only. No
// forwarding, search-domain resolution, credentials, or external upstreams.
export function dnsReply(query: Buffer, mode: Mode): Buffer | undefined {
  if (query.length < 17 || query.readUInt16BE(4) !== 1) return;
  let offset = 12;
  const labels: string[] = [];
  while (offset < query.length && query[offset] !== 0) {
    const length = query[offset++];
    if (length > 63 || offset + length >= query.length) return;
    labels.push(query.subarray(offset, offset + length).toString("ascii"));
    offset += length;
  }
  offset++;
  if (offset + 4 > query.length) return;
  const type = query.readUInt16BE(offset);
  const known = labels.join(".").toLowerCase() === HOST;
  if (known && mode === "drop") return;
  const answer = known && mode === "healthy" && type === 1;
  const response = Buffer.alloc(offset + 4 + (answer ? 16 : 0));
  query.copy(response, 0, 0, offset + 4);
  response.writeUInt16BE(known && mode === "healthy" ? 0x8180 : 0x8183, 2);
  response.writeUInt16BE(answer ? 1 : 0, 6);
  response.writeUInt16BE(0, 8);
  response.writeUInt16BE(0, 10);
  if (answer) {
    Buffer.from("c00c000100010000000000047f000001", "hex").copy(response, offset + 4);
  }
  return response;
}

async function run() {
  const token = process.env.MTC_PROBE_SERVICE_TOKEN;
  assert.ok(token && token.length >= 32, "missing synthetic bootstrap token");
  let mode: Mode = "healthy";
  let queries = 0;
  let posts = 0;
  const udp = dgram.createSocket("udp4");
  udp.on("message", (query, remote) => {
    if (isProbeQuery(query)) queries++;
    const response = dnsReply(query, mode);
    if (response) udp.send(response, remote.port, remote.address);
  });
  const tcp = net.createServer(socket => {
    let buffered = Buffer.alloc(0);
    socket.on("data", data => {
      buffered = Buffer.concat([buffered, data]);
      if (buffered.length < 2) return;
      const length = buffered.readUInt16BE(0);
      if (buffered.length < length + 2) return;
      if (isProbeQuery(buffered.subarray(2, length + 2))) queries++;
      const response = dnsReply(buffered.subarray(2, length + 2), mode);
      if (response) {
        const prefix = Buffer.alloc(2);
        prefix.writeUInt16BE(response.length);
        socket.end(Buffer.concat([prefix, response]));
      }
    });
    socket.setTimeout(5_000, () => socket.destroy());
  });
  const mock = http.createServer((request, response) => {
    request.resume();
    response.setHeader("content-type", "application/json");
    if (request.method === "POST") {
      posts++;
      response.end(JSON.stringify({
        id: "chatcmpl-synthetic", object: "chat.completion", created: 1,
        model: "dns-probe", choices: [{
          index: 0, message: { role: "assistant", content: "pong" }, finish_reason: "stop",
        }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
      }));
    } else {
      response.end(JSON.stringify({
        object: "list", data: [{ id: "dns-probe", object: "model", owned_by: "synthetic" }],
      }));
    }
  });
  await Promise.all([
    new Promise<void>(resolve => udp.bind(53, "127.0.0.1", resolve)),
    new Promise<void>(resolve => tcp.listen(53, "127.0.0.1", resolve)),
    new Promise<void>(resolve => mock.listen(18080, "127.0.0.1", resolve)),
  ]);

  const base = "http://127.0.0.1:8080";
  async function control(path: string, body: unknown) {
    stage = "configure_synthetic_fixtures";
    const result = await fetch(base + path, {
      method: "POST", headers: {
        authorization: `Bearer ${token}`, "content-type": "application/json",
      }, body: JSON.stringify(body), signal: AbortSignal.timeout(15_000),
    });
    assert.ok(result.ok, `synthetic setup rejected: ${path}, status=${result.status}`);
    return await result.json() as Record<string, any>;
  }
  try {
    stage = "readiness";
    let ready = false;
    for (let attempt = 0; attempt < 60; attempt++) {
      ready = await fetch(base + "/readyz", { signal: AbortSignal.timeout(1_000) })
        .then(result => result.ok).catch(() => false);
      if (ready) break;
      await sleep(1_000);
    }
    assert.ok(ready, "isolated MTC failed readiness");
    // Separate accounts prevent a prior negative phase's circuit breaker
    // from bypassing DNS in a later phase. Bootstrap all while DNS is healthy.
    const fixtures = [];
    for (const phase of ["healthy", "drop", "nxdomain"] as const) {
      const account = await control("/internal/v1/upstreams", {
        name: `dns-${phase}`, driver: "http-json",
        config: { base_url: `http://${HOST}:18080`, timeout_seconds: 15 },
        credential: { type: "api_key", value: CANARY },
      });
      const route = await control("/internal/v1/model-routes", {
        public_model: `dns-${phase}`, upstream_account_id: account.id,
        upstream_model: "dns-probe", protocol: "openai", custom_model_confirmed: true,
      });
      await control(`/internal/v1/prices/USD/dns-${phase}`, {
        input_per_million: "0", output_per_million: "0",
      });
      const key = await control("/internal/v1/keys", {
        principal_external_id: "synthetic-dns-probe", alias: `dns-${phase}`,
        route_ids: [route.id], policy: { enforcement_mode: "metered_unlimited" },
      });
      assert.equal(typeof account.id, "string");
      assert.equal(typeof key.key, "string");
      fixtures.push({ phase, account: account.id, key: key.key });
    }
    for (const fixture of fixtures) {
      stage = `request_${fixture.phase}`;
      mode = fixture.phase;
      const beforePosts = posts;
      const beforeQueries = queries;
      const started = performance.now();
      const result = await fetch(base + "/v1/chat/completions", {
        method: "POST", headers: {
          authorization: `Bearer ${fixture.key}`, "content-type": "application/json",
        }, body: JSON.stringify({
          model: `dns-${fixture.phase}`, messages: [{ role: "user", content: "ping" }],
          max_tokens: 4, stream: false,
        }), signal: AbortSignal.timeout(45_000),
      });
      await result.arrayBuffer();
      assert.ok(queries > beforeQueries, "DNS was bypassed; probe is not valid");
      if (fixture.phase === "healthy") {
        assert.equal(result.status, 200, "positive control failed");
        assert.equal(posts - beforePosts, 1, "positive control did not reach mock");
      } else {
        assert.ok(result.status >= 400, "DNS failure unexpectedly succeeded");
        assert.equal(posts - beforePosts, 0, "POST dispatched despite DNS failure");
      }
      console.log(JSON.stringify({
        phase: fixture.phase, account_id: fixture.account, status: result.status,
        dns_queries: queries - beforeQueries, posts: posts - beforePosts,
        elapsed_ms: Math.round(performance.now() - started),
      }));
    }
    // Diagnostics MUST additionally be checked from MTC container logs.
    console.log(JSON.stringify({ transport_checks: "passed", log_verification: "required" }));
  } finally {
    udp.close();
    tcp.close();
    mock.close();
    mock.closeAllConnections();
  }
}

if (isMain(import.meta.url, process.argv[1])) {
  run().catch(() => {
    // fetch/assert exceptions can contain tokens or URLs; never print them.
    console.error(JSON.stringify({ error: "synthetic DNS runtime probe failed", stage }));
    process.exitCode = 1;
  });
}
