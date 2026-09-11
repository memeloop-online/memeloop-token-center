#!/usr/bin/env node
/**
 * Read-only retirement gate for moving the canonical Token Center data plane
 * out of a reversible trial namespace.  This program intentionally has no
 * apply/delete mode: it inventories metadata only and produces a count-only
 * receipt that a separate, owner-approved change can attach to its ledger.
 */

import { createHash } from "node:crypto";
import { closeSync, constants, fstatSync, openSync, readSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { pathToFileURL } from "node:url";

/** Minimal strict JSON parser: JSON.parse accepts duplicate object fields,
 * which would let a receipt hide a failing value behind a later one. */
class StrictJsonReader {
  #offset = 0;
  readonly #text: string;
  constructor(text: string) { this.#text = text; }
  parse(): unknown { const value = this.value(); this.space(); if (this.#offset !== this.#text.length) throw new Error("trailing JSON data"); return value; }
  private space(): void { while (/\s/u.test(this.#text[this.#offset] ?? "")) this.#offset += 1; }
  private value(): unknown {
    this.space(); const current = this.#text[this.#offset];
    if (current === "{") return this.object(); if (current === "[") return this.array(); if (current === "\"") return this.string();
    if (this.#text.startsWith("true", this.#offset)) { this.#offset += 4; return true; }
    if (this.#text.startsWith("false", this.#offset)) { this.#offset += 5; return false; }
    if (this.#text.startsWith("null", this.#offset)) { this.#offset += 4; return null; }
    const number = this.#text.slice(this.#offset).match(/^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/u)?.[0];
    if (!number) throw new Error("invalid JSON value"); this.#offset += number.length;
    const result = Number(number); if (!Number.isFinite(result)) throw new Error("invalid JSON number"); return result;
  }
  private object(): Record<string, unknown> {
    this.#offset += 1; this.space(); const result: Record<string, unknown> = {}; const names = new Set<string>();
    if (this.#text[this.#offset] === "}") { this.#offset += 1; return result; }
    while (true) {
      this.space(); if (this.#text[this.#offset] !== "\"") throw new Error("object key expected"); const key = this.string();
      if (names.has(key)) throw new Error("duplicate JSON key"); names.add(key); this.space(); if (this.#text[this.#offset] !== ":") throw new Error("object colon expected"); this.#offset += 1; result[key] = this.value(); this.space();
      const separator = this.#text[this.#offset]; if (separator === "}") { this.#offset += 1; return result; } if (separator !== ",") throw new Error("object separator expected"); this.#offset += 1;
    }
  }
  private array(): unknown[] {
    this.#offset += 1; this.space(); const result: unknown[] = []; if (this.#text[this.#offset] === "]") { this.#offset += 1; return result; }
    while (true) { result.push(this.value()); this.space(); const separator = this.#text[this.#offset]; if (separator === "]") { this.#offset += 1; return result; } if (separator !== ",") throw new Error("array separator expected"); this.#offset += 1; }
  }
  private string(): string {
    const start = this.#offset; this.#offset += 1;
    while (this.#offset < this.#text.length) { const current = this.#text[this.#offset++]!; if (current === "\"") { const encoded = this.#text.slice(start, this.#offset); const decoded: unknown = JSON.parse(encoded); if (typeof decoded !== "string") throw new Error("invalid JSON string"); return decoded; } if (current === "\\") { const escaped = this.#text[this.#offset++]; if (escaped === "u") this.#offset += 4; else if (!escaped || !'"\\/bfnrt'.includes(escaped)) throw new Error("invalid JSON escape"); } else if (current < " ") throw new Error("control character in JSON string"); }
    throw new Error("unterminated JSON string");
  }
}
function parseStrictJson(text: string): unknown { return new StrictJsonReader(text).parse(); }

const MAX_RECEIPT_BYTES = 1024 * 1024;
const SHA256 = /^[0-9a-f]{64}$/u;
const DNS_LABEL = /^[a-z0-9]([-a-z0-9]*[a-z0-9])?$/u;
const HOST = /^(?=.{1,253}$)[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+$/u;
const RESOURCE_KINDS = [
  "deployments.apps",
  "statefulsets.apps",
  "daemonsets.apps",
  "jobs.batch",
  "cronjobs.batch",
  "services",
  "ingresses.networking.k8s.io",
  "persistentvolumeclaims",
  "networkpolicies.networking.k8s.io",
  "clusters.postgresql.cnpg.io",
  "pools.postgresql.cnpg.io",
] as const;

export class RetirementGateError extends Error {}
export type Resource = Readonly<{ apiVersion: string; kind: string; name: string; uid: string }>;
export type NamespaceInventory = Readonly<{ namespace: string; resources: readonly Resource[]; digest: string }>;
export type RetirementReceipt = Readonly<{
  schemaVersion: 1;
  canonicalNamespace: string;
  trialNamespace: string;
  operatorHost: string;
  finalWriteBarrier: boolean;
  postgres: Readonly<{ restored: boolean; catchupVerified: boolean; sourceWalCheckpointDigest: string; canonicalWalCheckpointDigest: string }>;
  objectStore: Readonly<{ restored: boolean; finalSyncVerified: boolean; sourceInventoryDigest: string; canonicalInventoryDigest: string }>;
  migration: Readonly<{
    cpampReconciled: boolean;
    archiveReconciled: boolean;
    noGapLocators: boolean;
    archiveCheckpoint: bigint;
    clientKeysExpected: bigint;
    clientKeysAttached: bigint;
    recoveryEnvelopesExpected: bigint;
    recoveryEnvelopesVerified: bigint;
  }>;
  operatorCutover: Readonly<{ canonicalHostServes: boolean; trialHostDetached: boolean }>;
  rollback: Readonly<{ canonicalRestoreTested: boolean; trialDataRetained: boolean }>;
  prune: Readonly<{ approved: boolean; retentionElapsed: boolean; trialDataExportVerified: boolean }>;
}>;

function fail(message: string): never { throw new RetirementGateError(message); }
function object(value: unknown, label: string): Record<string, unknown> {
  if (value === null || Array.isArray(value) || typeof value !== "object") fail(`${label} must be an object`);
  return value as Record<string, unknown>;
}
function string(value: unknown, label: string): string {
  if (typeof value !== "string" || value.length === 0 || /[\0\r\n]/u.test(value)) fail(`${label} is invalid`);
  return value;
}
function boolean(value: unknown, label: string): boolean {
  if (typeof value !== "boolean") fail(`${label} must be boolean`);
  return value;
}
function count(value: unknown, label: string): bigint {
  if (typeof value !== "number" && typeof value !== "string") fail(`${label} must be an unsigned integer`);
  const text = String(value);
  if (!/^\d+$/u.test(text)) fail(`${label} must be an unsigned integer`);
  return BigInt(text);
}
function digest(value: unknown, label: string): string {
  const text = string(value, label);
  if (!SHA256.test(text)) fail(`${label} must be a lowercase SHA-256 digest`);
  return text;
}
function namespace(value: unknown, label: string): string {
  const text = string(value, label);
  if (!DNS_LABEL.test(text) || text.length > 63) fail(`${label} is not a Kubernetes namespace`);
  return text;
}
function host(value: unknown, label: string): string {
  const text = string(value, label).toLowerCase();
  if (!HOST.test(text)) fail(`${label} is not a DNS host`);
  return text;
}
function exactKeys(value: Record<string, unknown>, expected: readonly string[], label: string): void {
  const actual = Object.keys(value).sort();
  const sorted = [...expected].sort();
  if (actual.length !== sorted.length || actual.some((key, index) => key !== sorted[index])) fail(`${label} has an unsupported schema`);
}

/** Parse a non-secret, immutable handoff receipt. Unknown fields are rejected. */
export function parseRetirementReceipt(raw: Buffer): RetirementReceipt {
  if (raw.length === 0 || raw.length > MAX_RECEIPT_BYTES) fail("retirement receipt has an invalid size");
  let value: unknown;
  try { value = parseStrictJson(raw.toString("utf8")); } catch { fail("retirement receipt is not strict JSON"); }
  const root = object(value, "retirement receipt");
  exactKeys(root, ["schema_version", "canonical_namespace", "trial_namespace", "operator_host", "final_write_barrier", "postgres", "object_store", "migration", "operator_cutover", "rollback", "prune"], "retirement receipt");
  if (root.schema_version !== 1) fail("retirement receipt schema_version is unsupported");
  const postgres = object(root.postgres, "postgres");
  exactKeys(postgres, ["restored", "catchup_verified", "source_wal_checkpoint_digest", "canonical_wal_checkpoint_digest"], "postgres");
  const objectStore = object(root.object_store, "object_store");
  exactKeys(objectStore, ["restored", "final_sync_verified", "source_inventory_digest", "canonical_inventory_digest"], "object_store");
  const migration = object(root.migration, "migration");
  exactKeys(migration, ["cpamp_reconciled", "archive_reconciled", "no_gap_locators", "archive_checkpoint", "client_keys_expected", "client_keys_attached", "recovery_envelopes_expected", "recovery_envelopes_verified"], "migration");
  const operatorCutover = object(root.operator_cutover, "operator_cutover");
  exactKeys(operatorCutover, ["canonical_host_serves", "trial_host_detached"], "operator_cutover");
  const rollback = object(root.rollback, "rollback");
  exactKeys(rollback, ["canonical_restore_tested", "trial_data_retained"], "rollback");
  const prune = object(root.prune, "prune");
  exactKeys(prune, ["approved", "retention_elapsed", "trial_data_export_verified"], "prune");
  const result: RetirementReceipt = {
    schemaVersion: 1,
    canonicalNamespace: namespace(root.canonical_namespace, "canonical_namespace"),
    trialNamespace: namespace(root.trial_namespace, "trial_namespace"),
    operatorHost: host(root.operator_host, "operator_host"),
    finalWriteBarrier: boolean(root.final_write_barrier, "final_write_barrier"),
    postgres: { restored: boolean(postgres.restored, "postgres.restored"), catchupVerified: boolean(postgres.catchup_verified, "postgres.catchup_verified"), sourceWalCheckpointDigest: digest(postgres.source_wal_checkpoint_digest, "postgres.source_wal_checkpoint_digest"), canonicalWalCheckpointDigest: digest(postgres.canonical_wal_checkpoint_digest, "postgres.canonical_wal_checkpoint_digest") },
    objectStore: { restored: boolean(objectStore.restored, "object_store.restored"), finalSyncVerified: boolean(objectStore.final_sync_verified, "object_store.final_sync_verified"), sourceInventoryDigest: digest(objectStore.source_inventory_digest, "object_store.source_inventory_digest"), canonicalInventoryDigest: digest(objectStore.canonical_inventory_digest, "object_store.canonical_inventory_digest") },
    migration: {
      cpampReconciled: boolean(migration.cpamp_reconciled, "migration.cpamp_reconciled"), archiveReconciled: boolean(migration.archive_reconciled, "migration.archive_reconciled"), noGapLocators: boolean(migration.no_gap_locators, "migration.no_gap_locators"), archiveCheckpoint: count(migration.archive_checkpoint, "migration.archive_checkpoint"), clientKeysExpected: count(migration.client_keys_expected, "migration.client_keys_expected"), clientKeysAttached: count(migration.client_keys_attached, "migration.client_keys_attached"), recoveryEnvelopesExpected: count(migration.recovery_envelopes_expected, "migration.recovery_envelopes_expected"), recoveryEnvelopesVerified: count(migration.recovery_envelopes_verified, "migration.recovery_envelopes_verified"),
    },
    operatorCutover: { canonicalHostServes: boolean(operatorCutover.canonical_host_serves, "operator_cutover.canonical_host_serves"), trialHostDetached: boolean(operatorCutover.trial_host_detached, "operator_cutover.trial_host_detached") },
    rollback: { canonicalRestoreTested: boolean(rollback.canonical_restore_tested, "rollback.canonical_restore_tested"), trialDataRetained: boolean(rollback.trial_data_retained, "rollback.trial_data_retained") },
    prune: { approved: boolean(prune.approved, "prune.approved"), retentionElapsed: boolean(prune.retention_elapsed, "prune.retention_elapsed"), trialDataExportVerified: boolean(prune.trial_data_export_verified, "prune.trial_data_export_verified") },
  };
  if (result.canonicalNamespace === result.trialNamespace) fail("canonical_namespace and trial_namespace must differ");
  return result;
}

export function inventoryFromKubectl(namespaceName: string, raw: string): NamespaceInventory {
  let value: unknown;
  try { value = parseStrictJson(raw); } catch { fail(`kubectl inventory for ${namespaceName} is invalid JSON`); }
  const root = object(value, `kubectl inventory for ${namespaceName}`);
  if (!Array.isArray(root.items)) fail(`kubectl inventory for ${namespaceName} has no items`);
  const resources = root.items.map((item, index): Resource => {
    const entry = object(item, `inventory item ${index}`);
    const metadata = object(entry.metadata, `inventory item ${index} metadata`);
    const apiVersion = string(entry.apiVersion, `inventory item ${index} apiVersion`);
    const kind = string(entry.kind, `inventory item ${index} kind`);
    const name = string(metadata.name, `inventory item ${index} name`);
    const uid = string(metadata.uid, `inventory item ${index} uid`);
    if (metadata.namespace !== namespaceName) fail(`inventory item ${index} is outside the selected namespace`);
    return { apiVersion, kind, name, uid };
  }).sort((left, right) => `${left.apiVersion}/${left.kind}/${left.name}`.localeCompare(`${right.apiVersion}/${right.kind}/${right.name}`));
  const canonical = JSON.stringify(resources);
  return { namespace: namespaceName, resources, digest: createHash("sha256").update(canonical).digest("hex") };
}

export type RetirementDecision = Readonly<{ canPromote: boolean; canPrune: boolean; promotionBlockers: readonly string[]; pruneBlockers: readonly string[] }>;
export function evaluateRetirement(receipt: RetirementReceipt, canonical: NamespaceInventory, trial: NamespaceInventory, requestedHost: string): RetirementDecision {
  const promotionBlockers: string[] = [];
  if (receipt.operatorHost !== host(requestedHost, "operator host")) promotionBlockers.push("operator host does not match the owner-approved receipt");
  if (canonical.namespace !== receipt.canonicalNamespace || trial.namespace !== receipt.trialNamespace) promotionBlockers.push("namespace inventory does not match the receipt");
  if (!receipt.finalWriteBarrier) promotionBlockers.push("final write barrier has not been proven");
  if (!receipt.postgres.restored || !receipt.postgres.catchupVerified || receipt.postgres.sourceWalCheckpointDigest !== receipt.postgres.canonicalWalCheckpointDigest) promotionBlockers.push("canonical PostgreSQL restore/catch-up or final WAL checkpoint is incomplete");
  if (!receipt.objectStore.restored || !receipt.objectStore.finalSyncVerified || receipt.objectStore.sourceInventoryDigest !== receipt.objectStore.canonicalInventoryDigest) promotionBlockers.push("canonical object-store restore/final inventory sync is incomplete");
  if (!receipt.migration.cpampReconciled || !receipt.migration.archiveReconciled || !receipt.migration.noGapLocators || receipt.migration.archiveCheckpoint === 0n) promotionBlockers.push("CPAMP/archive reconciliation is incomplete");
  if (receipt.migration.clientKeysExpected === 0n || receipt.migration.clientKeysExpected !== receipt.migration.clientKeysAttached) promotionBlockers.push("client-key continuity does not reconcile");
  if (receipt.migration.recoveryEnvelopesExpected === 0n || receipt.migration.recoveryEnvelopesExpected !== receipt.migration.recoveryEnvelopesVerified) promotionBlockers.push("recovery-envelope continuity does not reconcile");
  if (!receipt.operatorCutover.canonicalHostServes || !receipt.operatorCutover.trialHostDetached) promotionBlockers.push("operator host cutover is incomplete");
  if (!receipt.rollback.canonicalRestoreTested || !receipt.rollback.trialDataRetained) promotionBlockers.push("rollback/retention proof is incomplete");
  const pruneBlockers: string[] = [];
  if (!receipt.prune.approved || !receipt.prune.retentionElapsed || !receipt.prune.trialDataExportVerified) pruneBlockers.push("trial namespace prune approval, retention, or export proof is absent");
  // The inventory is intentionally retained in the receipt. A prune executor
  // must compare these exact UIDs again; this audit never deletes anything.
  if (trial.resources.some((item) => item.kind === "PersistentVolumeClaim" || item.kind === "Cluster" || item.kind === "Pool")) pruneBlockers.push("trial data resources remain protected; this read-only gate cannot prune them");
  return { canPromote: promotionBlockers.length === 0, canPrune: promotionBlockers.length === 0 && pruneBlockers.length === 0, promotionBlockers, pruneBlockers };
}

function readReceipt(path: string): Buffer {
  let fd: number | undefined;
  try {
    fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW);
    const metadata = fstatSync(fd);
    if (!metadata.isFile() || metadata.size > MAX_RECEIPT_BYTES) fail("retirement receipt must be a regular file no larger than 1 MiB");
    const result = Buffer.alloc(metadata.size);
    let offset = 0; while (offset < result.length) { const read = readSync(fd, result, offset, result.length - offset, null); if (read === 0) fail("retirement receipt changed while being read"); offset += read; }
    return result;
  } catch (error) { if (error instanceof RetirementGateError) throw error; fail("retirement receipt is not readable"); }
  finally { if (fd !== undefined) closeSync(fd); }
  throw new RetirementGateError("retirement receipt is not readable");
}
function kubectlInventory(binary: string, namespaceName: string): NamespaceInventory {
  const result = spawnSync(binary, ["-n", namespaceName, "get", RESOURCE_KINDS.join(","), "-o", "json"], { encoding: "utf8", shell: false, stdio: ["ignore", "pipe", "inherit"] });
  if (result.error || result.status !== 0) fail(result.error ? "kubectl is unavailable" : `kubectl inventory failed for ${namespaceName}`);
  return inventoryFromKubectl(namespaceName, String(result.stdout));
}
function args(argv: readonly string[]): Readonly<{ canonical: string; trial: string; operatorHost: string; receipt: string; kubectl: string }> {
  const values: Record<string, string> = {};
  const fields: Record<string, string> = { "--canonical-namespace": "canonical", "--trial-namespace": "trial", "--operator-host": "operatorHost", "--receipt": "receipt", "--kubectl-binary": "kubectl" };
  for (let index = 0; index < argv.length; index += 1) { const field = fields[argv[index] ?? ""]; const value = argv[index + 1]; if (!field || !value) fail("usage: audit-canonical-retirement --canonical-namespace NS --trial-namespace NS --operator-host HOST --receipt FILE [--kubectl-binary FILE]"); values[field] = value; index += 1; }
  if (!values.canonical || !values.trial || !values.operatorHost || !values.receipt) fail("usage: audit-canonical-retirement --canonical-namespace NS --trial-namespace NS --operator-host HOST --receipt FILE [--kubectl-binary FILE]");
  return { canonical: namespace(values.canonical, "canonical namespace"), trial: namespace(values.trial, "trial namespace"), operatorHost: host(values.operatorHost, "operator host"), receipt: values.receipt, kubectl: values.kubectl ?? "kubectl" };
}
function main(): void {
  const options = args(process.argv.slice(2));
  const receipt = parseRetirementReceipt(readReceipt(options.receipt));
  const canonical = kubectlInventory(options.kubectl, options.canonical);
  const trial = kubectlInventory(options.kubectl, options.trial);
  const decision = evaluateRetirement(receipt, canonical, trial, options.operatorHost);
  const kindCounts = (inventory: NamespaceInventory): Record<string, number> => Object.fromEntries([...new Set(inventory.resources.map((item) => item.kind))].sort().map((kind) => [kind, inventory.resources.filter((item) => item.kind === kind).length]));
  // Resource names/UIDs are deliberately retained so a future, separately
  // approved cleanup can compare exact targets. Secret resources and their
  // contents are never requested or emitted by this program.
  process.stdout.write(`${JSON.stringify({ mode: "dry-run", canonical_inventory: { digest: canonical.digest, resources: canonical.resources, kinds: kindCounts(canonical) }, trial_inventory: { digest: trial.digest, resources: trial.resources, kinds: kindCounts(trial) }, can_promote: decision.canPromote, can_prune: decision.canPrune, promotion_blockers: decision.promotionBlockers, prune_blockers: decision.pruneBlockers })}\n`);
  if (!decision.canPromote) process.exitCode = 1;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  try { main(); } catch (error) { process.stderr.write(`${error instanceof Error ? error.message : "retirement audit failed"}\n`); process.exitCode = 2; }
}
