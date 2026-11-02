#!/usr/bin/env node
/** Exact-revision release-process RSS gate. Mock/client allocations live in a
 * different PID and never contribute to the service's /proc RSS evidence. */
import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { accessSync, constants, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
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
export const NATIVE_MMAP_THRESHOLD_BYTES = 64 * 1024;
type Result = { status: number; bytes: number; sha256: string; requestId?: string; retryAfter?: string; transportClosed?: boolean };
type Plan = { prefix: Buffer; fillBytes: number; suffix: Buffer; bytes: number; path?: string };
type PendingReceipt = {
  request_id: string; receipt_id: string; usage_basis: string; status_code: number; error_code: string;
  response_object: string; claim: Record<string, unknown>; operational_terminal: Record<string, unknown>;
  reservation: { id: string; reserved_micros: number };
};
type ArchiveGapBaseline = {
  successful_request_archive_gaps: number; successful_response_archive_gaps: number;
  request_persistence: Record<string, number>; persistence: Record<string, number>;
};
const SAFE_UPSTREAM_ERROR = '{"error":{"code":"upstream_invalid_response","message":"上游请求失败。上游响应无效。","message_en":"upstream request failed. the upstream response was invalid.","type":"upstream_error"}}';

export function archiveDrainReady(row: Record<string, unknown>, retainedRequestCipherBytes?: number): boolean {
  const expected = retainedRequestCipherBytes === undefined ? 0 : 1;
  const cipherBytes = retainedRequestCipherBytes ?? 0;
  assert(Number.isSafeInteger(cipherBytes) && cipherBytes >= 0, "retained archive bytes must be valid");
  return row.pending_requests === expected && row.unsettled_reservations === expected
    && row.cipher_bytes === cipherBytes && row.request_cipher_bytes === cipherBytes;
}

export function assertUnknownFinancialEvidence(row: Record<string, unknown>, receipt: PendingReceipt): number {
  for (const field of ["financial_completed", "financial_status", "actual_micros", "receipt_id", "receipt_digest", "cost_micros"]) {
    assert(row[field] === null, `unknown must not fabricate financial completion: ${field}`);
  }
  assert(row.reservation_status === "reserved" && row.state === "pending", "unknown must retain its original financial hold");
  assert(row.unknown_reason === "authoritative_usage_unavailable", "unknown responsibility must be annotated");
  for (const field of ["ledger_count", "event_count", "fact_count"]) {
    assert(row[field] === 0, `unknown must have no financial side effects: ${field}`);
  }
  assert(row.operational_status === 502 && row.operational_error === "upstream_invalid_response", "real upstream failure must remain visible");
  assert(row.operational_stage === "buffered_response_ready", "failure must retain its actual stage");
  assert(typeof row.operational_completed === "number" && row.operational_completed > 0, "operational completion must be recorded");
  assert(receipt.request_id === row.request_id && receipt.receipt_id === row.operational_receipt_id, "journal must match the operational receipt");
  assert(receipt.usage_basis === "not_observed", "unknown must not fabricate provider usage");
  for (const field of ["request_id", "tenant_id", "key_id", "account_id", "reservation_id", "owner_token", "fence", "contract_digest"]) {
    assert(typeof row[field] === "string" && row[field] !== "" && receipt.claim[field] === row[field], `journal must retain exact claim authority: ${field}`);
  }
  assert(receipt.reservation.id === row.reservation_id && receipt.reservation.reserved_micros === row.reserved_micros, "journal must retain the original reservation");
  assert(receipt.status_code === row.operational_status && receipt.error_code === row.operational_error, "journal must retain the real client failure");
  const operational = receipt.operational_terminal;
  for (const [field, column] of Object.entries({ status_code: "operational_status", error_code: "operational_error", completed_at: "operational_completed", stage: "operational_stage", response_object: "operational_response_object" })) {
    assert(operational[field] === row[column], `operational ACK must match owned receipt: ${field}`);
  }
  assert(receipt.response_object === `inline-json:${SAFE_UPSTREAM_ERROR}` && operational.response_object === receipt.response_object, "failed response archive must contain only the safe local error");
  return assertUnknownArchiveEvidence(row);
}

export function assertUnknownArchiveEvidence(row: Record<string, unknown>): number {
  if (row.spool_state === "pending") {
    assert(row.spool_plain_bytes === 512 && row.chunk_plain_bytes === 512, "unknown request archive must remain durable, not be discarded");
    assert(typeof row.spool_cipher_bytes === "number" && Number.isSafeInteger(row.spool_cipher_bytes) && row.spool_cipher_bytes > 0, "retained request archive must have exact positive accounting");
    return row.spool_cipher_bytes;
  }
  // 61d gateway sources are already archived; encrypted spool accounting does
  // not own their bytes. Prove the source binding and cleanup ownership first.
  assert(row.spool_state === "bound", "unknown request archive must remain durable, not be discarded");
  assert(row.spool_tenant_id === row.tenant_id && row.spool_reservation_id === row.reservation_id, "source archive must retain exact financial ownership");
  const locator = `staging/proxy/${row.request_id}/request/${row.request_id}/body.json`;
  assert(row.request_object === locator && row.bound_locator === locator, "source archive must retain its canonical matching locator");
  assert(row.body_byte_count === 512 && typeof row.body_blake3 === "string" && /^[0-9a-f]{64}$/u.test(row.body_blake3), "source archive must retain body metadata");
  assert(row.spool_plain_bytes === 0 && row.spool_cipher_bytes === 0 && row.spool_chunk_count === 0 && row.chunk_count === 0 && row.chunk_plain_bytes === null, "source archive must not fabricate encrypted spool accounting");
  assert(row.staging_attempt_id === row.request_id && row.staging_owner_id === row.request_id
    && row.staging_owner_kind === "proxy_request" && row.staging_purpose === "request"
    && row.staging_writer_owner === "gateway-source" && row.staging_writer_token === row.request_id
    && row.staging_intent_digest === row.body_blake3 && row.staging_state === "cleanup_pending"
    && row.staging_bound_locator === null && typeof row.staging_bound_at === "number" && row.staging_bound_at > 0,
  "source cleanup must retain the actual gateway ownership and matching digest");
  return 0;
}

export function assertUnknownSemanticEvidence(row: Record<string, unknown>, previous?: Record<string, unknown>): Record<string, unknown> {
  assert(row.observation_count === 1, "unknown must project exactly one conversation observation");
  assert(row.projected_event_count === 1, "unknown must publish exactly one semantic projected event");
  for (const field of ["projection_completed", "operational_completed"]) {
    assert(typeof row[field] === "number" && row[field] > 0, "semantic projection must commit with operational evidence");
    if (previous) assert(row[field] === previous[field], `semantic replay must preserve timestamp: ${field}`);
  }
  assert(row.projection_observed_at === row.operational_completed, "semantic projection must use its operational receipt timestamp");
  for (const [field, owner] of Object.entries({
    observation_request_id: "request_id", observation_key_id: "key_id",
    cluster_tenant_id: "tenant_id", cluster_principal_id: "principal_id",
    observation_cluster_id: "request_cluster_id",
    projection_tenant_id: "tenant_id", projection_key_id: "key_id", projection_principal_id: "principal_id",
    projected_event_tenant_id: "tenant_id", projected_event_key_id: "key_id",
  })) {
    assert(typeof row[owner] === "string" && row[owner] !== "" && row[field] === row[owner], `semantic projection must retain exact ownership: ${field}`);
  }
  const evidence = Object.fromEntries([
    "request_id", "tenant_id", "key_id", "principal_id", "operational_receipt_id",
    "observation_id", "observation_cluster_id", "projected_event_id",
  ].map((field) => {
    assert(typeof row[field] === "string" && row[field] !== "", `semantic evidence missing: ${field}`);
    if (previous) assert(row[field] === previous[field], `semantic replay must preserve identity: ${field}`);
    return [field, row[field]];
  }));
  return { ...evidence, observation_count: 1, projected_event_count: 1, projection_completed: row.projection_completed, operational_completed: row.operational_completed };
}

function pendingReceipt(directory: string, receiptId: string): PendingReceipt | undefined {
  try {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      try { return JSON.parse(readFileSync(join(directory, entry.name, `${receiptId}.json`), "utf8")); }
      catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
    }
  } catch (error) { if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error; }
  return undefined;
}

export function nativeAllocatorEnvironment(): NodeJS.ProcessEnv {
  return { GLIBC_TUNABLES: `glibc.malloc.mmap_threshold=${NATIVE_MMAP_THRESHOLD_BYTES}` };
}

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
    deferred_jobs: 'memeloop_token_center_deferred_persistence_jobs',
    deferred_bytes: 'memeloop_token_center_deferred_persistence_bytes',
    deferred_stream_bytes: 'memeloop_token_center_deferred_stream_memory_bytes',
    request_jobs: 'memeloop_token_center_request_persistence_jobs',
    request_bytes: 'memeloop_token_center_request_persistence_bytes',
  };
  return Object.fromEntries(Object.entries(series).map(([name, prefix]) => {
    const line = metrics.split("\n").find((entry) => entry.startsWith(`${prefix} `));
    assert(line, `required permit gauge absent: ${name}`);
    const value = Number(line.slice(prefix.length + 1));
    assert(Number.isFinite(value) && value >= 0, `invalid permit gauge: ${name}`);
    return [name, value];
  }));
}

export function deferredPersistenceEvidence(metrics: string): Record<string, number> {
  return persistenceEvidence(metrics, "deferred", ["accepted", "capacity", "failed"]);
}

export function requestPersistenceEvidence(metrics: string): Record<string, number> {
  return persistenceEvidence(metrics, "request", ["accepted", "capacity", "failed", "retention_limit"]);
}

function persistenceEvidence(metrics: string, queue: string, outcomes: string[]): Record<string, number> {
  return Object.fromEntries(outcomes.map((outcome) => {
    const prefix = `memeloop_token_center_${queue}_persistence_total{outcome="${outcome}"}`;
    const line = metrics.split("\n").find((entry) => entry.startsWith(`${prefix} `));
    assert(line, `required ${queue} persistence counter absent: ${outcome}`);
    const value = Number(line.slice(prefix.length + 1));
    assert(Number.isSafeInteger(value) && value >= 0, `invalid ${queue} persistence counter: ${outcome}`);
    return [outcome, value];
  }));
}

export function assertArchiveGapEvidence(requestGaps: number, responseGaps: number, request: Record<string, number>, response: Record<string, number>): void {
  assert(request.failed === 0 && response.failed === 0, "healthy archive storage must not report capture failures");
  assert(request.retention_limit === 0, "acceptance inputs must not exceed the request retention limit");
  for (const [purpose, gaps, evidence] of [["request", requestGaps, request], ["response", responseGaps, response]] as const) {
    assert(Number.isSafeInteger(gaps) && gaps >= 0, `${purpose} archive gap count must be valid`);
    const capacity = evidence.capacity;
    assert(capacity !== undefined && Number.isSafeInteger(capacity) && capacity >= 0, `${purpose} capacity rejection counter must be present and valid`);
    assert(gaps <= capacity, `${purpose} archive gaps (${gaps}) must be accounted for by explicit bounded capacity rejection (${capacity})`);
  }
}

export function assertArchiveReplayGapEvidence(requestGaps: number, responseGaps: number, request: Record<string, number>, response: Record<string, number>, baseline: ArchiveGapBaseline): void {
  assertArchiveGapEvidence(baseline.successful_request_archive_gaps, baseline.successful_response_archive_gaps, baseline.request_persistence, baseline.persistence);
  assert(requestGaps === baseline.successful_request_archive_gaps, "journal replay must preserve the previously accounted request archive gaps");
  assert(responseGaps === baseline.successful_response_archive_gaps, "journal replay must preserve the previously accounted response archive gaps");
  assertArchiveGapEvidence(0, 0, request, response);
}

export function allocatorEvidence(metrics: string): Record<string, number> {
  const states = ["allocated", "active", "resident", "mapped", "retained"];
  return Object.fromEntries(states.map((state) => {
    const prefix = `memeloop_token_center_allocator_bytes{state="${state}"}`;
    const line = metrics.split("\n").find((entry) => entry.startsWith(`${prefix} `));
    assert(line, `required allocator gauge absent: ${state}`);
    const value = Number(line.slice(prefix.length + 1));
    assert(Number.isFinite(value) && value >= 0, `invalid allocator gauge: ${state}`);
    return [state, value];
  }));
}

export function nativeAllocatorEvidence(metrics: string): Record<string, number> {
  const states = ["arena", "allocated", "free", "mmap", "releasable"];
  return Object.fromEntries(states.map((state) => {
    const prefix = `memeloop_token_center_native_allocator_bytes{state="${state}"}`;
    const line = metrics.split("\n").find((entry) => entry.startsWith(`${prefix} `));
    assert(line, `required native allocator gauge absent: ${state}`);
    const value = Number(line.slice(prefix.length + 1));
    assert(Number.isFinite(value) && value >= 0, `invalid native allocator gauge: ${state}`);
    return [state, value];
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
      response.once("end", () => done({ status: response.statusCode ?? 0, bytes, sha256: hash.digest("hex"), requestId: String(response.headers["x-mtc-request-id"] ?? ""), retryAfter: String(response.headers["retry-after"] ?? "") }));
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
  const requestSpool = join(work, "request-spool");
  mkdirSync(archive);
  mkdirSync(requestSpool);
  const report: Record<string, any> = {
    benchmark: "durable-archive-release-process-rss", binary,
    binary_sha256: createHash("sha256").update(readFileSync(binary)).digest("hex"),
    started_at: new Date().toISOString(), limit_mib: LIMIT_MIB,
    native_allocator_environment: nativeAllocatorEnvironment(),
    pod_budget_mib: 512, reserved_runtime_headroom_mib: 64,
    isolation: "independent_process_not_cgroup",
    evidence_source: "independent service PID /proc/status RSS partition, /proc/smaps_rollup mapping attribution, kernel VmHWM, jemalloc, and glibc main-arena metrics; mock/client excluded",
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
    Object.assign(env, nativeAllocatorEnvironment(), {
      MTC_LISTEN: `127.0.0.1:${serviceAddress.port}`, MTC_DATABASE_URL: `sqlite://${database}?mode=rwc`,
      MTC_DATABASE_MAX_CONNECTIONS: "2", MTC_SERVICE_TOKEN: token,
      MTC_KEY_PEPPER: "durable-rss-pepper-has-at-least-thirty-two-bytes",
      MTC_ARCHIVE_BACKEND: "filesystem", MTC_ARCHIVE_PATH: archive,
      MTC_RESPONSES_REQUEST_SPOOL_PATH: requestSpool,
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
    const idleProcessMemory = processMemory(service.pid);
    const idle = idleProcessMemory.rss_mib as number;
    report.idle_rss_mib = idle;
    report.idle_process_memory = idleProcessMemory;
    const idleMetrics = await apiRequest(base, "GET", "/metrics", token, undefined, 2000);
    assert(idleMetrics.status === 200, "idle allocator metrics must be available");
    report.idle_allocator_bytes = allocatorEvidence(idleMetrics.body.toString("utf8"));
    report.idle_native_allocator_bytes = nativeAllocatorEvidence(idleMetrics.body.toString("utf8"));

    const drain = async (unknownRequestId?: string, previousSemantics?: Record<string, unknown>, archiveGapBaseline?: ArchiveGapBaseline): Promise<Record<string, unknown>> => {
      const reader = new DatabaseSync(database, { readOnly: true });
      reader.exec("PRAGMA busy_timeout = 1000");
      try {
        return await deadline((async () => {
          while (true) {
            const drainEvidence: Record<string, unknown> = { request_id: unknownRequestId, waiting_for: "financial_and_archive_state" };
            report.last_archive_drain = drainEvidence;
            let retainedRequestCipherBytes: number | undefined;
            let unknownEvidence: Record<string, unknown> | undefined;
            if (unknownRequestId !== undefined) {
              const unknown = reader.prepare(`
                SELECT p.*, q.completed_at AS financial_completed, q.status_code AS financial_status,
                  r.status AS reservation_status, r.actual_micros, r.reserved_micros,
                  o.status_code AS operational_status, o.error_code AS operational_error,
                  o.completed_at AS operational_completed, o.stage AS operational_stage,
                  o.receipt_id AS operational_receipt_id, o.response_object AS operational_response_object,
                  q.request_object, s.tenant_id AS spool_tenant_id, s.reservation_id AS spool_reservation_id,
                  s.state AS spool_state, s.byte_count AS spool_plain_bytes, s.cipher_bytes AS spool_cipher_bytes,
                  s.chunk_count AS spool_chunk_count, s.bound_locator, s.body_byte_count, s.body_blake3,
                  a.attempt_id AS staging_attempt_id, a.owner_id AS staging_owner_id,
                  a.owner_kind AS staging_owner_kind, a.purpose AS staging_purpose,
                  a.writer_owner AS staging_writer_owner, a.writer_token AS staging_writer_token,
                  a.intent_digest AS staging_intent_digest, a.state AS staging_state,
                  a.bound_locator AS staging_bound_locator, a.bound_at AS staging_bound_at,
                  (SELECT COUNT(*) FROM request_archive_spool_chunks WHERE request_id = q.id) AS chunk_count,
                  (SELECT SUM(byte_count) FROM request_archive_spool_chunks WHERE request_id = q.id) AS chunk_plain_bytes,
                  (SELECT COUNT(*) FROM ledger_entries WHERE source = q.reservation_id) AS ledger_count,
                  (SELECT COUNT(*) FROM request_events WHERE request_id = q.id AND event_kind = 'finished') AS event_count,
                  (SELECT COUNT(*) FROM request_stats_facts WHERE request_id = q.id) AS fact_count,
                  (SELECT COUNT(*) FROM conversation_observations WHERE request_id = q.id) AS observation_count,
                  (SELECT COUNT(*) FROM request_events WHERE request_id = q.id AND event_kind = 'projected') AS projected_event_count,
                  k.principal_id, q.conversation_cluster_id AS request_cluster_id,
                  v.id AS observation_id, v.request_id AS observation_request_id,
                  v.key_id AS observation_key_id, v.cluster_id AS observation_cluster_id,
                  c.tenant_id AS cluster_tenant_id, c.principal_id AS cluster_principal_id,
                  x.tenant_id AS projection_tenant_id, x.key_id AS projection_key_id,
                  x.principal_id AS projection_principal_id, x.projected_at AS projection_completed,
                  x.observed_at AS projection_observed_at,
                  e.event_id AS projected_event_id, e.tenant_id AS projected_event_tenant_id,
                  e.key_id AS projected_event_key_id
                FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id
                JOIN key_records k ON k.id = q.key_id AND k.tenant_id = q.tenant_id
                JOIN pending_proxy_settlements p ON p.request_id = q.id AND p.reservation_id = r.id
                  AND p.tenant_id = q.tenant_id AND p.key_id = q.key_id AND p.account_id = r.account_id
                LEFT JOIN proxy_operational_terminals o ON o.request_id = p.request_id AND o.fence = p.fence AND o.contract_digest = p.contract_digest
                  AND o.tenant_id = p.tenant_id AND o.key_id = p.key_id AND o.account_id = p.account_id
                  AND o.reservation_id = p.reservation_id AND o.owner_token = p.owner_token
                LEFT JOIN request_archive_spools s ON s.request_id = p.request_id AND s.reservation_id = p.reservation_id
                LEFT JOIN archive_staging_attempts a ON a.attempt_id = q.id
                LEFT JOIN conversation_observations v ON v.request_id = q.id
                LEFT JOIN conversation_clusters c ON c.id = v.cluster_id
                LEFT JOIN conversation_projection_outbox x ON x.request_id = q.id
                LEFT JOIN request_events e ON e.request_id = q.id AND e.event_kind = 'projected'
                WHERE q.id = ?`).get(unknownRequestId);
              const receipt = typeof unknown?.operational_receipt_id === "string"
                ? pendingReceipt(join(requestSpool, "pending-terminals-v1"), unknown.operational_receipt_id) : undefined;
              drainEvidence.unknown = unknown ? Object.fromEntries([
                "state", "unknown_reason", "financial_completed", "reservation_status",
                "operational_status", "operational_error", "operational_completed", "operational_receipt_id",
                "spool_state", "spool_plain_bytes", "spool_cipher_bytes", "chunk_plain_bytes",
                "observation_count", "projected_event_count", "projection_completed",
              ].map((field) => [field, unknown[field]])) : null;
              drainEvidence.durable_receipt_found = receipt !== undefined;
              if (!unknown || unknown.unknown_reason === null || !receipt || unknown.spool_state === null || unknown.spool_state === "capturing" || unknown.spool_state === "uploading") {
                await delay(100); continue;
              }
              retainedRequestCipherBytes = assertUnknownFinancialEvidence(unknown, receipt);
              if (unknown.spool_state === "pending") { await delay(100); continue; }
              if (unknown.projection_completed === null) { await delay(100); continue; }
              const semantics = assertUnknownSemanticEvidence(unknown, previousSemantics);
              const archived = await apiRequest(base, "GET", `/self/v1/requests/${unknownRequestId}/archive/request`, key, undefined, 5000);
              const archive = {
                locator: unknown.bound_locator, body_byte_count: unknown.body_byte_count,
                body_blake3: unknown.body_blake3, status: archived.status, bytes: archived.body.length,
                sha256: createHash("sha256").update(archived.body).digest("hex"),
              };
              assert(archive.status === 200 && archive.bytes === 512 && archive.sha256 === planHash(inputPlan(512)), "unknown request archive GET must return the exact original 512-byte body/hash");
              if (archiveGapBaseline) {
                const prior = (archiveGapBaseline as ArchiveGapBaseline & { unknown_financial_responsibility?: { archive?: typeof archive } }).unknown_financial_responsibility?.archive;
                assert(prior && JSON.stringify(prior) === JSON.stringify(archive), "unknown request archive replay must preserve body metadata, locator and raw bytes/hash");
              }
              unknownEvidence = {
                request_id: unknownRequestId, receipt_id: receipt.receipt_id, fence: unknown.fence,
                financial_state: "unknown", status_code: unknown.operational_status,
                error_code: unknown.operational_error, stage: unknown.operational_stage,
                completed_at: unknown.operational_completed, reservation_status: unknown.reservation_status,
                reserved_micros: unknown.reserved_micros, retained_request_cipher_bytes: retainedRequestCipherBytes,
                durable_journal: true, financial_ack: false, financial_side_effects: 0,
                semantics, archive,
              };
            }
            const row = reader.prepare("SELECT cipher_bytes, request_cipher_bytes, (SELECT COUNT(*) FROM request_records WHERE completed_at IS NULL) AS pending_requests, (SELECT COUNT(*) FROM usage_reservations WHERE status = 'reserved') AS unsettled_reservations FROM response_archive_spool_budget WHERE singleton = 1").get();
            assert(row, "archive budget row missing");
            drainEvidence.budget = row;
            if (archiveDrainReady(row, retainedRequestCipherBytes)) {
              drainEvidence.waiting_for = "memory_and_worker_permits";
              const metrics = await apiRequest(base, "GET", "/metrics", token, undefined, 2000);
              assert(metrics.status === 200, "permit metrics must be available");
              const metricsText = metrics.body.toString("utf8");
              const gauges = permitEvidence(metricsText);
              drainEvidence.permits = gauges;
              if (Object.values(gauges).every((value) => value === 0)) {
                const currentBudget = reader.prepare("SELECT cipher_bytes, request_cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1").get();
                assert(currentBudget, "archive budget row missing after capture drain");
                if (!archiveDrainReady({ ...row, ...currentBudget }, retainedRequestCipherBytes)) { await delay(100); continue; }
                const successfulGaps = reader.prepare("SELECT COALESCE(SUM(CASE WHEN request_object LIKE 'gap:%' THEN 1 ELSE 0 END), 0) AS request_gaps, COALESCE(SUM(CASE WHEN response_object IS NULL OR response_object LIKE 'gap:%' THEN 1 ELSE 0 END), 0) AS response_gaps FROM request_records WHERE status_code = 200").get();
                const persistence = deferredPersistenceEvidence(metricsText);
                const requestPersistence = requestPersistenceEvidence(metricsText);
                const requestGaps = Number(successfulGaps?.request_gaps);
                const responseGaps = Number(successfulGaps?.response_gaps);
                if (archiveGapBaseline) {
                  assertArchiveReplayGapEvidence(requestGaps, responseGaps, requestPersistence, persistence, archiveGapBaseline);
                } else {
                  assertArchiveGapEvidence(requestGaps, responseGaps, requestPersistence, persistence);
                }
                const processMemoryEvidence = processMemory(service!.pid!);
                return { ...row, unknown_financial_responsibility: unknownEvidence, permits: gauges, persistence, request_persistence: requestPersistence, rss_mib: processMemoryEvidence.rss_mib, process_memory: processMemoryEvidence, allocator_bytes: allocatorEvidence(metricsText), native_allocator_bytes: nativeAllocatorEvidence(metricsText), successful_archive_gaps: requestGaps + responseGaps, successful_request_archive_gaps: requestGaps, successful_response_archive_gaps: responseGaps };
              }
            }
            await delay(100);
          }
        })(), 30_000, "archive drain");
      } finally { reader.close(); }
    };
    const phase = async (name: string, operation: () => Promise<unknown>, expectUnknown = false): Promise<void> => {
      const callsBefore = upstreamCalls;
      const from = report.samples.length;
      const operationStarted = performance.now();
      const entry = { name, result: undefined as unknown, upstream_calls: 0, peak_rss_mib: 0, operation_duration_ms: 0, drain_duration_ms: 0, drain: {} };
      report.phases.push(entry);
      try {
        entry.result = await deadline(operation(), 60_000, name);
      } finally {
        entry.operation_duration_ms = performance.now() - operationStarted;
        entry.upstream_calls = upstreamCalls - callsBefore;
        sample();
        entry.peak_rss_mib = Math.max(...report.samples.slice(from).map((s: any) => s.rss_mib));
      }
      assert(entry.peak_rss_mib <= LIMIT_MIB, `${name}: service RSS exceeded 448 MiB (512 MiB pod minus 64 MiB headroom)`);
      const drainStarted = performance.now();
      const unknownRequestId = expectUnknown ? (entry.result as Result).requestId : undefined;
      if (expectUnknown) assert(unknownRequestId, "failed client response must identify its unknown financial responsibility");
      entry.drain = await drain(unknownRequestId);
      entry.drain_duration_ms = performance.now() - drainStarted;
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
      assert(result.bytes === Buffer.byteLength(SAFE_UPSTREAM_ERROR) && result.sha256 === createHash("sha256").update(SAFE_UPSTREAM_ERROR).digest("hex"), "malformed upstream response must expose only the exact safe local error");
      underdeclaredResponse = false;
      return result;
    }, true);
    const peak = Math.max(...report.samples.map((s: any) => Math.max(s.rss_mib, s.high_water_mib)));
    report.peak_rss_or_kernel_high_water_mib = peak;
    assert(peak <= LIMIT_MIB, "kernel VmHWM exceeded 448 MiB service allowance");
    let recovered = Infinity;
    let cooldownComplete = false;
    try {
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
      cooldownComplete = true;
    } finally {
      try {
        const cooldownMetrics = await apiRequest(base, "GET", "/metrics", token, undefined, 2000);
        assert(cooldownMetrics.status === 200, "cooldown allocator metrics must be available");
        const cooldownMetricsText = cooldownMetrics.body.toString("utf8");
        report.cooldown_allocator_bytes = allocatorEvidence(cooldownMetricsText);
        report.cooldown_native_allocator_bytes = nativeAllocatorEvidence(cooldownMetricsText);
        report.cooldown_process_memory = processMemory(service!.pid!);
      } catch (error) {
        report.cooldown_allocator_error = error instanceof Error ? error.message : String(error);
        if (cooldownComplete) throw error;
      }
    }
    report.recovered_rss_mib = recovered;
    report.duration_seconds = (performance.now() - started) / 1000;
    assert(!sampleFailure && memoryVerdict(idle, peak, recovered), "independent-process RSS peak/recovery gate failed");
    const beforeReplayDrain = report.phases.at(-1).drain;
    const beforeReplay = beforeReplayDrain.unknown_financial_responsibility;
    const callsBeforeReplay = upstreamCalls;
    if (sampler) clearInterval(sampler);
    sampler = undefined;
    await stop(service);
    const replayLog = `${output}.replay.log`;
    writeFileSync(replayLog, "");
    service = spawn(binary, ["serve", "--role", "all"], { env, stdio: ["ignore", "pipe", "pipe"] });
    service.once("error", (error) => { sampleFailure = error; });
    service.stdout?.on("data", (chunk: Buffer) => writeFileSync(replayLog, chunk, { flag: "a" }));
    service.stderr?.on("data", (chunk: Buffer) => writeFileSync(replayLog, chunk, { flag: "a" }));
    await deadline((async () => {
      while (true) {
        assert(!sampleFailure && service!.exitCode === null, "service exited during unknown receipt replay");
        const replayed = readFileSync(replayLog, "utf8").split("\n").some((line) => {
          let entry; try { entry = JSON.parse(line); } catch { return false; }
          return entry.fields?.message === "terminal lacks authoritative provider usage; receipt retained without zero-usage settlement"
            && entry.fields?.request_id === beforeReplay.request_id && entry.fields?.receipt_id === beforeReplay.receipt_id;
        });
        if (replayed) break;
        await delay(100);
      }
    })(), 30_000, "unknown receipt replay");
    const replayDrain = await drain(beforeReplay.request_id, beforeReplay.semantics, beforeReplayDrain);
    assert(upstreamCalls === callsBeforeReplay, "unknown journal replay must not dispatch upstream");
    report.unknown_receipt_replay = { service_pid: service.pid, upstream_calls: upstreamCalls - callsBeforeReplay, drain: replayDrain };
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
