import assert from "node:assert/strict";
import test from "node:test";
import { evaluateRetirement, inventoryFromKubectl, parseRetirementReceipt } from "../../ops/release/audit-canonical-retirement.ts";

const digest = "a".repeat(64);
function receipt(overrides: Record<string, unknown> = {}) {
  return Buffer.from(JSON.stringify({
    schema_version: 1, canonical_namespace: "token-center", trial_namespace: "token-center-trial", operator_host: "operator.example.test", final_write_barrier: true,
    postgres: { restored: true, catchup_verified: true, source_wal_checkpoint_digest: digest, canonical_wal_checkpoint_digest: digest }, object_store: { restored: true, final_sync_verified: true, source_inventory_digest: digest, canonical_inventory_digest: digest },
    migration: { cpamp_reconciled: true, archive_reconciled: true, no_gap_locators: true, archive_checkpoint: 1, client_keys_expected: 11, client_keys_attached: 11, recovery_envelopes_expected: 11, recovery_envelopes_verified: 11 },
    operator_cutover: { canonical_host_serves: true, trial_host_detached: true }, rollback: { canonical_restore_tested: true, trial_data_retained: true }, prune: { approved: true, retention_elapsed: true, trial_data_export_verified: true }, ...overrides,
  }));
}
function inventory(namespace: string, items: unknown[] = []) { return inventoryFromKubectl(namespace, JSON.stringify({ items })); }

test("retirement audit accepts only count-complete migration evidence and never allows PVC pruning", () => {
  const good = parseRetirementReceipt(receipt());
  const canonical = inventory("token-center", [{ apiVersion: "v1", kind: "Service", metadata: { namespace: "token-center", name: "gateway", uid: "a" } }]);
  const trial = inventory("token-center-trial", [{ apiVersion: "v1", kind: "PersistentVolumeClaim", metadata: { namespace: "token-center-trial", name: "pg-data", uid: "b" } }]);
  const result = evaluateRetirement(good, canonical, trial, "operator.example.test");
  assert.equal(result.canPromote, true);
  assert.equal(result.canPrune, false);
  assert.ok(result.pruneBlockers.some((item) => item.includes("data resources remain protected")));
});

test("a missing recovery envelope or an archive gap blocks promotion", () => {
  const missingEnvelope = parseRetirementReceipt(receipt({ migration: { cpamp_reconciled: true, archive_reconciled: true, no_gap_locators: true, archive_checkpoint: 1, client_keys_expected: 11, client_keys_attached: 11, recovery_envelopes_expected: 11, recovery_envelopes_verified: 10 } }));
  const result = evaluateRetirement(missingEnvelope, inventory("token-center"), inventory("token-center-trial"), "operator.example.test");
  assert.equal(result.canPromote, false);
  assert.ok(result.promotionBlockers.some((item) => item.includes("recovery-envelope")));
  assert.throws(() => parseRetirementReceipt(receipt({ migration: { cpamp_reconciled: true, archive_reconciled: true, no_gap_locators: false, archive_checkpoint: 1, client_keys_expected: 11, client_keys_attached: 11, recovery_envelopes_expected: 11, recovery_envelopes_verified: 11 }, unexpected: true })), /unsupported schema/);
});

test("a lagging final WAL checkpoint blocks canonical promotion", () => {
  const lagging = parseRetirementReceipt(receipt({ postgres: { restored: true, catchup_verified: true, source_wal_checkpoint_digest: digest, canonical_wal_checkpoint_digest: "b".repeat(64) } }));
  const result = evaluateRetirement(lagging, inventory("token-center"), inventory("token-center-trial"), "operator.example.test");
  assert.equal(result.canPromote, false);
  assert.ok(result.promotionBlockers.some((item) => item.includes("WAL checkpoint")));
});
