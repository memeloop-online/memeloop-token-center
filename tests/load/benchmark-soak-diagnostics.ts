import { DatabaseSync } from "node:sqlite";

/** Only synthetic harness metadata, never credential/config/request payloads. */
export function readSoakFailureClassification(databasePath: string, since: number): Record<string, unknown> {
  let database: DatabaseSync | undefined;
  try {
    database = new DatabaseSync(databasePath, { readOnly: true, timeout: 250 });
    const tenant = database.prepare("SELECT id FROM tenants WHERE external_id = ?").get("memory-benchmark");
    if (!tenant || typeof tenant.id !== "string") return { available: false, reason: "benchmark_tenant_missing" };
    const requests = database.prepare(
      "SELECT id, status_code, error_code, created_at, completed_at, upstream_account_id FROM request_records WHERE tenant_id = ? AND created_at >= ? AND status_code >= 500 ORDER BY created_at DESC, id DESC LIMIT 10",
    ).all(tenant.id, since);
    const health = database.prepare(
      "SELECT h.upstream_account_id, h.consecutive_failures, h.cooldown_until, h.last_failure_kind, h.updated_at FROM upstream_account_health h JOIN upstream_accounts a ON a.id = h.upstream_account_id WHERE a.tenant_id = ? AND h.updated_at >= ? ORDER BY h.updated_at DESC LIMIT 10",
    ).all(tenant.id, since);
    return { available: true, requests, health };
  } catch {
    // A diagnostic read must not change or mask the original failure count.
    return { available: false, reason: "classification_read_unavailable" };
  } finally {
    database?.close();
  }
}

export class FirstSoakFailureEvidence {
  private captured = false;
  value: Record<string, unknown> | undefined;

  capture(read: () => Record<string, unknown>): void {
    if (this.captured) return;
    this.captured = true;
    try { this.value = { captured_at: new Date().toISOString(), ...read() }; }
    catch { this.value = { available: false, reason: "first_failure_capture_unavailable" }; }
  }
}
