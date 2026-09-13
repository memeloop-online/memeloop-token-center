#!/usr/bin/env node
/** Exact-revision release-process RSS gate. Mock/client allocations live in a
 * different PID and never contribute to the service's /proc RSS evidence. */
import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { accessSync, constants, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, request, type ClientRequest, type ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { setTimeout as delay } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import { gzipSync } from "node:zlib";
import { apiRequest, MIB, processMemory, seed } from "./benchmark-memory.ts";

// This gate measures an independent process, not a cgroup. Keep 64MiB of
// explicit pod/runtime headroom within the declared 512MiB deployment budget.
const LIMIT_MIB = 448;
const CHUNK = 64 * 1024;
type Result = { status: number; bytes: number; sha256: string; retryAfter?: string; transportClosed?: boolean };
type Plan = { prefix: Buffer; fillBytes: number; suffix: Buffer; bytes: number; path?: string };

export function responsesInputPlan(bytes: number): Plan {
  const prefix = Buffer.from('{"model":"benchmark-text","input":"');
  const suffix = Buffer.from('","max_output_tokens":1,"stream":false}');
  assert(bytes >= prefix.length + suffix.length, "Responses input plan too small");
  return { prefix, suffix, fillBytes: bytes - prefix.length - suffix.length, bytes, path: "/v1/responses" };
}

export function responsesOutputPlan(bytes: number): Plan {
  const prefix = Buffer.from('{"id":"resp-rss","object":"response","status":"completed","model":"benchmark-text","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"');
  const suffix = Buffer.from('"}]}],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}}');
  assert(bytes >= prefix.length + suffix.length, "Responses output plan too small");
  return { prefix, suffix, fillBytes: bytes - prefix.length - suffix.length, bytes };
}

export function requestPath(plan: Plan): string { return plan.path ?? "/v1/chat/completions"; }

export function inputPlan(bytes: number): Plan {
  const prefix = Buffer.from('{"model":"benchmark-text","messages":[{"role":"user","content":"');
  const suffix = Buffer.from('"}],"max_tokens":1}');
  if (bytes < prefix.length + suffix.length) throw new Error("input plan too small");
  return { prefix, suffix, fillBytes: bytes - prefix.length - suffix.length, bytes };
}

export function outputPlan(bytes: number): Plan {
  const prefix = Buffer.from('{"id":"chatcmpl-rss","object":"chat.completion","model":"benchmark-text","choices":[{"index":0,"message":{"role":"assistant","content":"');
  const suffix = Buffer.from('"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}');
  if (bytes < prefix.length + suffix.length) throw new Error("output plan too small");
  return { prefix, suffix, fillBytes: bytes - prefix.length - suffix.length, bytes };
}

function* pieces(plan: Plan): Generator<Buffer> {
  yield plan.prefix;
  const block = Buffer.alloc(CHUNK, "x");
  for (let left = plan.fillBytes; left > 0; left -= CHUNK) yield block.subarray(0, Math.min(CHUNK, left));
  yield plan.suffix;
}

export function planHash(plan: Plan): string {
  const hash = createHash("sha256");
  for (const piece of pieces(plan)) hash.update(piece);
  return hash.digest("hex");
}

export function memoryVerdict(idle: number, peak: number, recovered: number): boolean {
  const requiredDrop = Math.min(16, Math.max(0, peak - idle) / 2);
  return peak <= LIMIT_MIB && recovered <= idle + 64 && recovered <= peak - requiredDrop;
}

export function permitEvidence(metrics: string): Record<string, number> {
  const series: Record<string, string> = {
    lifecycle_memory: 'memeloop_token_center_proxy_memory_bytes{pool="lifecycle",measure="used"}',
    retained_request_memory: 'memeloop_token_center_proxy_memory_bytes{pool="retained_request",measure="used"}',
    body_readers: 'memeloop_token_center_background_work_items{queue="gateway_body_reads",state="active"}',
    lifecycles: 'memeloop_token_center_background_work_items{queue="proxy_lifecycles",state="active"}',
    archive_workers: 'memeloop_token_center_background_work_items{queue="proxy_archive_streams",state="active"}',
  };
  return Object.fromEntries(Object.entries(series).map(([name, prefix]) => {
    const line = metrics.split("\n").find((entry) => entry.startsWith(`${prefix} `));
    assert(line, `required permit gauge absent: ${name}`);
    const value = Number(line.slice(prefix.length + 1));
    assert(Number.isFinite(value) && value >= 0, `invalid permit gauge: ${name}`);
    return [name, value];
  }));
}

function assert(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}

function gate(): { promise: Promise<void>; release: () => void } {
  let release!: () => void;
  const promise = new Promise<void>((done) => { release = done; });
  return { promise, release };
}

async function deadline<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} exceeded ${ms} ms`)), ms);
    })]);
  } finally { if (timer) clearTimeout(timer); }
}

async function writeResponse(response: ServerResponse, plan: Plan, chunked: boolean): Promise<void> {
  response.writeHead(200, { "content-type": "application/json", ...(chunked ? {} : { "content-length": String(plan.bytes) }) });
  for (const piece of pieces(plan)) {
    if (response.destroyed) return;
    if (!response.write(piece)) await writableBoundary(response);
  }
  response.end();
}

function writableBoundary(stream: ClientRequest | ServerResponse): Promise<void> {
  return new Promise<void>((done, reject) => {
    const cleanup = (): void => {
      stream.off("drain", finish); stream.off("close", finish);
      stream.off("response", finish); stream.off("error", fail);
    };
    const finish = (): void => { cleanup(); done(); };
    const fail = (error: Error): void => { cleanup(); reject(error); };
    stream.once("drain", finish); stream.once("close", finish);
    stream.once("response", finish); stream.once("error", fail);
  });
}

/** The client streams both directions; it does not buffer a 64MiB response. */
function send(base: string, key: string, plan: Plan, options: {
  chunked?: boolean; contentLength?: number; encoding?: string; raw?: Buffer; expectContinue?: boolean;
} = {}): { uploaded: Promise<void>; result: Promise<Result> } {
  const uploaded = gate();
  const target = new URL(requestPath(plan), base);
  let stopped = false;
  const result = new Promise<Result>((done, reject) => {
    const req = request(target, { method: "POST", headers: {
      authorization: `Bearer ${key}`, "content-type": "application/json", connection: "close",
      ...(options.chunked ? {} : { "content-length": String(options.contentLength ?? options.raw?.length ?? plan.bytes) }),
      ...(options.encoding ? { "content-encoding": options.encoding } : {}),
      ...(options.expectContinue ? { expect: "100-continue" } : {}),
    } }, (response) => {
      stopped = true;
      uploaded.release();
      const hash = createHash("sha256");
      let bytes = 0;
      response.on("data", (chunk: Buffer) => { bytes += chunk.length; hash.update(chunk); });
      response.once("error", reject);
      response.once("end", () => done({ status: response.statusCode ?? 0, bytes, sha256: hash.digest("hex"), retryAfter: String(response.headers["retry-after"] ?? "") }));
    });
    req.setTimeout(45_000, () => req.destroy(new Error("proxy request timed out")));
    req.once("error", (error) => { stopped = true; uploaded.release(); reject(error); });
    req.once("finish", uploaded.release);
    const uploadPermission = gate();
    if (options.expectContinue) {
      // A known-oversized request must let the gateway answer from its headers
      // before the client writes a body the gateway is required to reject.
      // If the gateway needs the body, HTTP/1.1 Continue opens the upload.
      req.once("continue", uploadPermission.release);
      req.once("response", uploadPermission.release);
      req.once("close", uploadPermission.release);
      req.once("error", uploadPermission.release);
    } else {
      uploadPermission.release();
    }
    req.flushHeaders();
    void (async () => {
      try {
        await uploadPermission.promise;
        for (const piece of options.raw ? [options.raw] : pieces(plan)) {
          if (stopped || req.destroyed) break;
          if (!req.write(piece)) await writableBoundary(req);
        }
        if (!req.destroyed) req.end();
      } catch (error) { req.destroy(error instanceof Error ? error : new Error("body upload failed")); }
    })();
  });
  // Attach a handler immediately: a deliberately rejected large upload may
  // fail before the phase has finished waiting for every upload boundary.
  void result.catch(() => {});
  return { uploaded: uploaded.promise, result };
}

async function stop(child: ChildProcess | undefined): Promise<void> {
  if (!child || child.exitCode !== null || !child.pid) return;
  const exited = once(child, "exit");
  child.kill("SIGTERM");
  try { await deadline(exited, 8000, "service shutdown"); }
  catch { child.kill("SIGKILL"); await deadline(exited, 2000, "service kill"); }
}

export async function run(binary: string, output: string): Promise<boolean> {
  accessSync(binary, constants.X_OK);
  assert(process.platform === "linux", "RSS acceptance requires Linux /proc");
  mkdirSync(dirname(output), { recursive: true });
  const work = mkdtempSync(join(tmpdir(), "mtc-durable-rss-"));
  const database = join(work, "service.db");
  const archive = join(work, "archive");
  mkdirSync(archive);
  const report: Record<string, any> = {
    benchmark: "durable-archive-release-process-rss", binary,
    binary_sha256: createHash("sha256").update(readFileSync(binary)).digest("hex"),
    started_at: new Date().toISOString(), limit_mib: LIMIT_MIB,
    pod_budget_mib: 512, reserved_runtime_headroom_mib: 64,
    isolation: "independent_process_not_cgroup",
    evidence_source: "independent service PID /proc/status VmRSS and kernel VmHWM; mock/client excluded",
    phases: [], samples: [], passed: false,
  };
  let service: ChildProcess | undefined;
  let sampler: NodeJS.Timeout | undefined;
  let sampleFailure: unknown;
  let current = outputPlan(512);
  let chunkedResponse = false;
  let underdeclaredResponse = false;
  let responseGate: ReturnType<typeof gate> | undefined;
  let upstreamCalls = 0;
  const mock = createServer(async (req, res) => {
    try {
      if (req.method === "GET") { res.end("{}"); return; }
      if (!["/v1/chat/completions", "/v1/responses"].includes(req.url ?? "")) { res.writeHead(404); res.end(); return; }
      upstreamCalls += 1;
      for await (const _ of req) { /* real upstream consumes the forwarded body */ }
      if (responseGate) await responseGate.promise;
      if (underdeclaredResponse) {
        // Real HTTP framing exposes only the declared JSON prefix to reqwest;
        // extra wire bytes are not permitted to become a replayed response.
        res.writeHead(200, { "content-type": "application/json", "content-length": "16" });
        res.end(Buffer.concat([...pieces(current)]));
        return;
      }
      await writeResponse(res, current, chunkedResponse);
    } catch { res.destroy(); }
  });
  mock.requestTimeout = 60_000;
  mock.keepAliveTimeout = 120_000;
  try {
    await new Promise<void>((done) => mock.listen(0, "127.0.0.1", done));
    const address = mock.address();
    assert(address && typeof address !== "string", "local mock port unavailable");
    const mockUrl = `http://127.0.0.1:${address.port}`;
    const portProbe = createServer();
    await new Promise<void>((done) => portProbe.listen(0, "127.0.0.1", done));
    const serviceAddress = portProbe.address();
    assert(serviceAddress && typeof serviceAddress !== "string", "service port unavailable");
    const base = `http://127.0.0.1:${serviceAddress.port}`;
    await new Promise<void>((done, reject) => portProbe.close((error) => error ? reject(error) : done()));
    const token = "local-durable-rss-bootstrap-only";
    const env: NodeJS.ProcessEnv = {};
    for (const name of ["PATH", "HOME", "LANG", "TMPDIR", "SSL_CERT_DIR", "SSL_CERT_FILE"]) {
      if (process.env[name] !== undefined) env[name] = process.env[name];
    }
    Object.assign(env, {
      MTC_LISTEN: `127.0.0.1:${serviceAddress.port}`, MTC_DATABASE_URL: `sqlite://${database}?mode=rwc`,
      MTC_DATABASE_MAX_CONNECTIONS: "2", MTC_SERVICE_TOKEN: token,
      MTC_KEY_PEPPER: "durable-rss-pepper-has-at-least-thirty-two-bytes",
      MTC_ARCHIVE_BACKEND: "filesystem", MTC_ARCHIVE_PATH: archive,
      MTC_ALLOW_OAUTH_LOOPBACK: "true", MTC_RUN_MIGRATIONS_ON_START: "false",
      MTC_PROXY_MEMORY_BUDGET_BYTES: String(256 * MIB), MTC_RESPONSES_BODY_MAX_BYTES: String(16 * MIB),
      MTC_PRICING_MODELS_DEV_URL: `${mockUrl}/catalog`, MTC_PRICING_LITELLM_URL: `${mockUrl}/catalog`,
      MTC_PRICING_OPENROUTER_URL: `${mockUrl}/catalog`, RUST_LOG: "warn",
    });
    const migration = spawnSync(binary, ["migrate"], { env, encoding: "utf8", timeout: 60_000 });
    writeFileSync(`${output}.migration.log`, `${migration.stdout ?? ""}${migration.stderr ?? ""}`);
    assert(migration.status === 0 && !migration.error, "release binary migration failed");
    service = spawn(binary, ["serve", "--role", "all"], { env, stdio: ["ignore", "pipe", "pipe"] });
    service.once("error", (error) => { sampleFailure = error; });
    writeFileSync(`${output}.service.log`, "");
    service.stdout?.on("data", (chunk: Buffer) => writeFileSync(`${output}.service.log`, chunk, { flag: "a" }));
    service.stderr?.on("data", (chunk: Buffer) => writeFileSync(`${output}.service.log`, chunk, { flag: "a" }));
    assert(service.pid, "release service did not start");
    report.service_pid = service.pid;
    report.mock_pid = process.pid;
    const sample = (): void => {
      try { report.samples.push({ at_ms: performance.now(), ...processMemory(service!.pid!) }); }
      catch (error) { sampleFailure = error; }
    };
    sample();
    sampler = setInterval(sample, 20);
    const started = performance.now();
    await deadline((async () => {
      while (true) {
        assert(service!.exitCode === null, "service exited during readiness");
        try { if ((await apiRequest(base, "GET", "/readyz", token, undefined, 1000)).status === 200) break; }
        catch { /* bounded readiness deadline below */ }
        await delay(100);
      }
    })(), 30_000, "readiness");
    // The seeded `openai` http-json route covers both Chat and Responses;
    // large ingress must use Responses' real 16MiB limit, not Chat's 4MiB limit.
    const key = await seed(base, base, token, mockUrl);
    const idle = processMemory(service.pid).rss_mib as number;
    report.idle_rss_mib = idle;

    const drain = async (): Promise<Record<string, unknown>> => {
      const reader = new DatabaseSync(database, { readOnly: true });
      reader.exec("PRAGMA busy_timeout = 1000");
      try {
        return await deadline((async () => {
          while (true) {
            const row = reader.prepare("SELECT cipher_bytes, request_cipher_bytes, (SELECT COUNT(*) FROM request_records WHERE completed_at IS NULL) AS pending_requests, (SELECT COUNT(*) FROM usage_reservations WHERE status = 'reserved') AS unsettled_reservations FROM response_archive_spool_budget WHERE singleton = 1").get();
            assert(row, "archive budget row missing");
            if (Object.values(row).every((value) => Number(value) === 0)) {
              const metrics = await apiRequest(base, "GET", "/metrics", token, undefined, 2000);
              assert(metrics.status === 200, "permit metrics must be available");
              const gauges = permitEvidence(metrics.body.toString("utf8"));
              if (Object.values(gauges).every((value) => value === 0)) {
                const successfulGaps = reader.prepare("SELECT COUNT(*) AS count FROM request_records WHERE status_code = 200 AND (request_object LIKE 'gap:%' OR response_object IS NULL OR response_object LIKE 'gap:%')").get();
                assert(Number(successfulGaps?.count) === 0, "successful buffered requests must converge both archives, not settle with a silent gap");
                return { ...row, permits: gauges, successful_archive_gaps: Number(successfulGaps?.count) };
              }
            }
            await delay(100);
          }
        })(), 30_000, "archive drain");
      } finally { reader.close(); }
    };
    const phase = async (name: string, operation: () => Promise<unknown>): Promise<void> => {
      const callsBefore = upstreamCalls;
      const from = report.samples.length;
      const result = await deadline(operation(), 60_000, name);
      sample();
      const peak = Math.max(...report.samples.slice(from).map((s: any) => s.rss_mib));
      const entry = { name, result, upstream_calls: upstreamCalls - callsBefore, peak_rss_mib: peak, drain: {} };
      report.phases.push(entry);
      assert(peak <= LIMIT_MIB, `${name}: service RSS exceeded 448 MiB (512 MiB pod minus 64 MiB headroom)`);
      entry.drain = await drain();
    };

    await phase("single-64MiB-buffered-response", async () => {
      current = outputPlan(64 * MIB);
      const result = await send(base, key, inputPlan(512)).result;
      assert(result.status === 200 && result.bytes === current.bytes && result.sha256 === planHash(current), "64MiB buffered response must reach client with exact bytes/hash");
      return result;
    });
    await phase("four-concurrent-16MiB-known-length-inputs", async () => {
      current = responsesOutputPlan(512);
      responseGate = gate();
      const callsBefore = upstreamCalls;
      const clients = Array.from({ length: 4 }, () => send(base, key, responsesInputPlan(16 * MIB)));
      try { await deadline(Promise.all(clients.map((client) => client.uploaded)), 30_000, "concurrent upload boundaries"); }
      finally { responseGate.release(); responseGate = undefined; }
      const results = await Promise.all(clients.map((client) => client.result));
      assert(results.every((item) => item.status === 200 || (item.status === 503 && Number(item.retryAfter) >= 1)), "pressure may fail closed only with 503 and Retry-After");
      assert(upstreamCalls - callsBefore === results.filter((item) => item.status === 200).length, "rejected pressure requests must not dispatch upstream");
      // Admission winners depend on scheduling; even an all-503 pressure batch
      // is valid. Require deterministic real work after every owner drains.
      await drain();
      const recovery = await send(base, key, responsesInputPlan(16 * MIB)).result;
      assert(recovery.status === 200 && recovery.bytes === current.bytes && recovery.sha256 === planHash(current), "single 16MiB recovery must succeed with exact response bytes");
      assert(upstreamCalls - callsBefore === results.filter((item) => item.status === 200).length + 1, "pressure plus recovery must execute exactly once per successful request");
      return { results, recovery };
    });
    await phase("unknown-chunked-16MiB-input-and-64MiB-response", async () => {
      current = responsesOutputPlan(64 * MIB); chunkedResponse = true;
      const result = await send(base, key, responsesInputPlan(16 * MIB), { chunked: true }).result;
      assert(result.status === 200 && result.bytes === current.bytes && result.sha256 === planHash(current), "unknown-length request/response must complete exact bytes");
      chunkedResponse = false;
      return result;
    });
    await phase("64MiB-input-default-policy-rejection", async () => {
      current = outputPlan(512);
      const before = upstreamCalls;
      const result = await send(base, key, responsesInputPlan(64 * MIB), { expectContinue: true }).result;
      assert([413, 503].includes(result.status) && upstreamCalls === before, "default 16MiB ingress boundary must reject 64MiB before upstream");
      if (result.status === 503) assert(Number(result.retryAfter) >= 1, "503 requires Retry-After");
      return result;
    });
    await phase("encoded-body-rejected-without-dispatch", async () => {
      const before = upstreamCalls;
      const plan = responsesInputPlan(16 * MIB);
      const compressed = gzipSync(Buffer.concat([...pieces(plan)]));
      const result = await send(base, key, plan, { encoding: "gzip", raw: compressed }).result;
      assert([400, 415].includes(result.status) && upstreamCalls === before, "encoded body must not bypass bounded JSON admission");
      return result;
    });
    await phase("underdeclared-content-length-never-dispatches", async () => {
      const before = upstreamCalls;
      const result = await send(base, key, responsesInputPlan(16 * MIB), { contentLength: 16 }).result.catch((error: NodeJS.ErrnoException) => {
        if (!["ECONNRESET", "EPIPE"].includes(error.code ?? "")) throw error;
        return { status: 0, bytes: 0, sha256: "", transportClosed: true };
      });
      assert([0, 400, 413, 503].includes(result.status) && upstreamCalls === before, "underdeclared body must not dispatch malformed JSON");
      return result;
    });
    await phase("upstream-underdeclared-json-fails-once-without-replay", async () => {
      const before = upstreamCalls;
      current = outputPlan(512); underdeclaredResponse = true;
      const result = await send(base, key, inputPlan(512)).result;
      report.malformed_upstream_observation = {
        status: result.status,
        response_bytes: result.bytes,
        upstream_calls: upstreamCalls - before,
      };
      assert(result.status === 502 && upstreamCalls - before === 1, "malformed upstream framing must fail once without replaying upstream");
      underdeclaredResponse = false;
      return result;
    });
    const peak = Math.max(...report.samples.map((s: any) => Math.max(s.rss_mib, s.high_water_mib)));
    report.peak_rss_or_kernel_high_water_mib = peak;
    assert(peak <= LIMIT_MIB, "kernel VmHWM exceeded 448 MiB service allowance");
    let recovered = Infinity;
    await deadline((async () => {
      let consecutive = 0;
      while (consecutive < 5) {
        assert(!sampleFailure, "service disappeared while sampling RSS");
        recovered = processMemory(service!.pid!).rss_mib;
        report.recovered_rss_mib = recovered;
        consecutive = memoryVerdict(idle, peak, recovered) ? consecutive + 1 : 0;
        await delay(200);
      }
    })(), 30_000, "real RSS cooldown recovery");
    report.recovered_rss_mib = recovered;
    report.duration_seconds = (performance.now() - started) / 1000;
    assert(!sampleFailure && memoryVerdict(idle, peak, recovered), "independent-process RSS peak/recovery gate failed");
    report.passed = true;
  } catch (error) {
    report.error = error instanceof Error ? error.message : String(error);
  } finally {
    responseGate?.release();
    if (sampler) clearInterval(sampler);
    await stop(service).catch((error) => { report.passed = false; report.shutdown_error = String(error); });
    mock.closeAllConnections();
    await new Promise<void>((done) => mock.close(() => done()));
    report.finished_at = new Date().toISOString();
    writeFileSync(output, `${JSON.stringify(report, null, 2)}\n`);
    // Only this invocation's freshly created scratch database/archive is removed.
    rmSync(work, { recursive: true, force: true });
  }
  return Boolean(report.passed);
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  const binary = resolve(process.argv[2] ?? "target/release/memeloop-token-center");
  const output = resolve(process.argv[3] ?? "tests/load/results/durable-archive-rss.json");
  try { process.exitCode = await run(binary, output) ? 0 : 1; }
  catch (error) { console.error(error instanceof Error ? error.message : String(error)); process.exitCode = 1; }
}
