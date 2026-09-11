import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';

const SCHEMA = 'mtc.native-cli-sealed-capture.v1';
const SHA256 = /^sha256:[0-9a-f]{64}$/u;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/u;
const RFC3339 = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/u;
const PROVIDERS = ['copilot', 'cursor'] as const;

type Provider = (typeof PROVIDERS)[number];
type JsonObject = Record<string, unknown>;
type Action = 'create_disabled_account_and_install_state' | 'install_state' | 'no_op';

interface SealedPackage {
  source_account_ref: string;
  provider: Provider;
  target_tenant_id: string;
  target_account_id: string;
  target_credential_generation: number;
  target_state_generation: number;
  target_provider_label: string;
  target_driver: string;
  auth_allowlist_digest: string;
  source_snapshot_uid: string;
  capture_lease_id: string;
  envelope: string;
  algorithm: string;
  key_reference_digest: string;
  aad_fields: string[];
  aad_digest: string;
  package_digest: string;
  plaintext_file_count: number;
  plaintext_bytes: number;
  ciphertext_bytes: number;
}

interface TargetState {
  target_tenant_id: string;
  target_account_id: string;
  provider: Provider;
  provider_label: string;
  driver: string;
  status: 'absent' | 'disabled';
  credential_generation: number;
  state_generation: number | null;
  installed_package_digest: string | null;
  route_candidate_count: number;
}

interface PlannedAction {
  action: Action;
  provider: Provider;
  idempotency_key: string;
}

function fail(code: string): never {
  throw new Error(`native CLI migration import dry-run rejected: ${code}`);
}

function object(value: unknown, code: string): JsonObject {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) fail(code);
  return value as JsonObject;
}

function exactKeys(value: JsonObject, keys: readonly string[], code: string): void {
  const actual = Object.keys(value).sort();
  const expected = [...keys].sort();
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) fail(code);
}

function string(value: unknown, code: string): string {
  if (typeof value !== 'string' || value.length === 0) fail(code);
  return value;
}

function pattern(value: unknown, expected: RegExp, code: string): string {
  const parsed = string(value, code);
  if (!expected.test(parsed)) fail(code);
  return parsed;
}

function boolean(value: unknown, expected: boolean, code: string): void {
  if (value !== expected) fail(code);
}

function integer(value: unknown, minimum: number, maximum: number, code: string): number {
  if (!Number.isSafeInteger(value) || (value as number) < minimum || (value as number) > maximum) fail(code);
  return value as number;
}

function timestamp(value: unknown, code: string): number {
  const parsed = pattern(value, RFC3339, code);
  const milliseconds = Date.parse(parsed);
  if (!Number.isFinite(milliseconds)) fail(code);
  return milliseconds;
}

function provider(value: unknown, code: string): Provider {
  if (value !== 'copilot' && value !== 'cursor') fail(code);
  return value;
}

function tenantId(value: unknown, code: string): string {
  const parsed = string(value, code);
  if (parsed.length > 200 || parsed.trim() !== parsed || [...parsed].some((character) => /\p{Cc}/u.test(character))) fail(code);
  return parsed;
}

function array(value: unknown, code: string): unknown[] {
  if (!Array.isArray(value)) fail(code);
  return value;
}

function nullableInteger(value: unknown, code: string): number | null {
  return value === null ? null : integer(value, 0, 2 ** 31 - 1, code);
}

function nullableDigest(value: unknown, code: string): string | null {
  return value === null ? null : pattern(value, SHA256, code);
}

function stableJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(',')}]`;
  if (value !== null && typeof value === 'object') {
    return `{${Object.entries(value as JsonObject).sort(([left], [right]) => left.localeCompare(right))
      .map(([key, child]) => `${JSON.stringify(key)}:${stableJson(child)}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

function digest(value: unknown, domain: string): string {
  return `sha256:${createHash('sha256').update(domain).update('\0').update(stableJson(value)).digest('hex')}`;
}

function rejectSecretShapedData(value: unknown): void {
  const forbiddenKeys = new Set([
    'access_token', 'refresh_token', 'id_token', 'password', 'cookie', 'authorization',
    'client_secret', 'private_key', 'credential', 'credentials', 'auth_json', 'plaintext',
    'ciphertext', 'nonce', 'key_material', 'state_archive', 'package_path',
  ]);
  const forbiddenValues = [
    /-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----/u,
    /\bBearer\s+[A-Za-z0-9._~+/=-]{12,}/u,
    /\b(?:gh[opusr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b/u,
    /\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b/u,
  ];
  const visit = (node: unknown): void => {
    if (typeof node === 'string') {
      if (forbiddenValues.some((expression) => expression.test(node))) fail('secret-shaped value is forbidden');
      return;
    }
    if (Array.isArray(node)) {
      for (const item of node) visit(item);
      return;
    }
    if (node === null || typeof node !== 'object') return;
    for (const [key, child] of Object.entries(node as JsonObject)) {
      if (forbiddenKeys.has(key.toLowerCase())) fail('secret-bearing field is forbidden');
      visit(child);
    }
  };
  visit(value);
}

const packageKeys = [
  'source_account_ref', 'provider', 'target_tenant_id', 'target_account_id',
  'target_credential_generation', 'target_state_generation', 'target_provider_label', 'target_driver',
  'auth_allowlist_digest', 'source_snapshot_uid', 'capture_lease_id', 'envelope',
  'algorithm', 'key_reference_digest', 'aad_fields', 'aad_digest', 'package_digest',
  'plaintext_file_count', 'plaintext_bytes', 'ciphertext_bytes',
] as const;

function parsePackage(value: unknown, index: number): SealedPackage {
  const item = object(value, `package ${index} must be an object`);
  exactKeys(item, packageKeys, `package ${index} has unknown or missing fields`);
  const selectedProvider = provider(item.provider, `package ${index} provider is invalid`);
  const expectedLabel = selectedProvider === 'copilot' ? 'Copilot' : 'Cursor';
  const expectedDriver = `${selectedProvider}-cli`;
  if (item.target_provider_label !== expectedLabel || item.target_driver !== expectedDriver) {
    fail(`package ${index} target identity is not native`);
  }
  if (item.envelope !== 'mtc-upstream-credential-v2' || item.algorithm !== 'chacha20-poly1305') {
    fail(`package ${index} encryption carrier is unsupported`);
  }
  const aadFields = array(item.aad_fields, `package ${index} aad fields must be an array`);
  if (stableJson(aadFields) !== stableJson(['tenant_external_id', 'account_id', 'provider', 'state_generation'])) {
    fail(`package ${index} aad binding is incomplete`);
  }
  const plaintextBytes = integer(item.plaintext_bytes, 1, 8 * 1024 * 1024, `package ${index} plaintext size is invalid`);
  const ciphertextBytes = integer(item.ciphertext_bytes, plaintextBytes + 16, 64 * 1024 * 1024, `package ${index} ciphertext size is invalid`);
  const targetTenantId = tenantId(item.target_tenant_id, `package ${index} tenant id is invalid`);
  const targetAccountId = pattern(item.target_account_id, UUID, `package ${index} target account id is invalid`);
  const targetCredentialGeneration = integer(item.target_credential_generation, 1, 2 ** 31 - 1, `package ${index} target generation is invalid`);
  const targetStateGeneration = integer(item.target_state_generation, 0, 2 ** 31 - 1, `package ${index} target state generation is invalid`);
  const aadDigest = pattern(item.aad_digest, SHA256, `package ${index} aad digest is invalid`);
  if (aadDigest !== digest({
    tenant_external_id: targetTenantId,
    account_id: targetAccountId,
    provider: selectedProvider,
    state_generation: targetStateGeneration,
  }, 'memeloop-token-center/native-cli-state/v1')) {
    fail(`package ${index} aad digest does not match its target binding`);
  }
  return {
    source_account_ref: pattern(item.source_account_ref, SHA256, `package ${index} account reference is invalid`),
    provider: selectedProvider,
    target_tenant_id: targetTenantId,
    target_account_id: targetAccountId,
    target_credential_generation: targetCredentialGeneration,
    target_state_generation: targetStateGeneration,
    target_provider_label: expectedLabel,
    target_driver: expectedDriver,
    auth_allowlist_digest: pattern(item.auth_allowlist_digest, SHA256, `package ${index} allowlist digest is invalid`),
    source_snapshot_uid: pattern(item.source_snapshot_uid, UUID, `package ${index} snapshot uid is invalid`),
    capture_lease_id: pattern(item.capture_lease_id, UUID, `package ${index} capture lease is invalid`),
    envelope: 'mtc-upstream-credential-v2',
    algorithm: 'chacha20-poly1305',
    key_reference_digest: pattern(item.key_reference_digest, SHA256, `package ${index} key reference is invalid`),
    aad_fields: ['tenant_external_id', 'account_id', 'provider', 'state_generation'],
    aad_digest: aadDigest,
    package_digest: pattern(item.package_digest, SHA256, `package ${index} package digest is invalid`),
    plaintext_file_count: integer(item.plaintext_file_count, 1, 512, `package ${index} file count is invalid`),
    plaintext_bytes: plaintextBytes,
    ciphertext_bytes: ciphertextBytes,
  };
}

const targetKeys = [
  'target_tenant_id', 'target_account_id', 'provider', 'provider_label', 'driver',
  'status', 'credential_generation', 'state_generation', 'installed_package_digest',
  'route_candidate_count',
] as const;

function parseTarget(value: unknown, index: number): TargetState {
  const item = object(value, `target ${index} must be an object`);
  exactKeys(item, targetKeys, `target ${index} has unknown or missing fields`);
  const selectedProvider = provider(item.provider, `target ${index} provider is invalid`);
  const expectedLabel = selectedProvider === 'copilot' ? 'Copilot' : 'Cursor';
  const expectedDriver = `${selectedProvider}-cli`;
  if (item.provider_label !== expectedLabel || item.driver !== expectedDriver) fail(`target ${index} identity is not native`);
  if (item.status !== 'absent' && item.status !== 'disabled') fail(`target ${index} must be absent or disabled`);
  const status = item.status;
  const credentialGeneration = integer(item.credential_generation, 0, 2 ** 31 - 1, `target ${index} credential generation is invalid`);
  const stateGeneration = nullableInteger(item.state_generation, `target ${index} state generation is invalid`);
  const installedPackageDigest = nullableDigest(item.installed_package_digest, `target ${index} installed package digest is invalid`);
  const routeCandidateCount = integer(item.route_candidate_count, 0, 0, `target ${index} is already routed`);
  if (status === 'absent' && (credentialGeneration !== 0 || stateGeneration !== null || installedPackageDigest !== null)) {
    fail(`target ${index} absent-state evidence is inconsistent`);
  }
  if (status === 'disabled' && credentialGeneration < 1) fail(`target ${index} disabled-state evidence is inconsistent`);
  if ((stateGeneration === null) !== (installedPackageDigest === null)) fail(`target ${index} state receipt is incomplete`);
  return {
    target_tenant_id: tenantId(item.target_tenant_id, `target ${index} tenant id is invalid`),
    target_account_id: pattern(item.target_account_id, UUID, `target ${index} account id is invalid`),
    provider: selectedProvider,
    provider_label: expectedLabel,
    driver: expectedDriver,
    status,
    credential_generation: credentialGeneration,
    state_generation: stateGeneration,
    installed_package_digest: installedPackageDigest,
    route_candidate_count: routeCandidateCount,
  };
}

function validateDocument(value: unknown): { packages: SealedPackage[]; targets: TargetState[]; actions: PlannedAction[]; preflightEvidenceDigest: string } {
  rejectSecretShapedData(value);
  const root = object(value, 'document must be an object');
  exactKeys(root, ['schema', 'preflight_receipt', 'capture', 'packages', 'target_inventory', 'import_fence', 'rollback'], 'document has unknown or missing fields');
  if (root.schema !== SCHEMA) fail('schema is unsupported');

  const preflight = object(root.preflight_receipt, 'preflight receipt must be an object');
  exactKeys(preflight, ['schema', 'status', 'evidence_digest', 'account_count', 'provider_counts', 'supplier_requests', 'quota_reset_requests'], 'preflight receipt has unknown or missing fields');
  if (preflight.schema !== 'mtc.native-cli-migration-preflight-receipt.v1' || preflight.status !== 'preflight_ready_for_sealed_capture') fail('preflight receipt is not ready');
  const preflightEvidenceDigest = pattern(preflight.evidence_digest, SHA256, 'preflight evidence digest is invalid');
  if (integer(preflight.supplier_requests, 0, 0, 'preflight supplier requests must be zero') !== 0 ||
      integer(preflight.quota_reset_requests, 0, 0, 'preflight reset requests must be zero') !== 0) fail('preflight included forbidden provider activity');
  const providerCounts = object(preflight.provider_counts, 'preflight provider counts must be an object');
  exactKeys(providerCounts, PROVIDERS, 'preflight provider counts are incomplete');

  const capture = object(root.capture, 'capture must be an object');
  exactKeys(capture, [
    'source_snapshot_uid', 'capture_lease_id', 'sealed_at', 'plaintext_destroyed_at',
    'source_mount_read_only', 'plaintext_persistence', 'plaintext_logging',
    'supplier_requests', 'quota_reset_requests',
  ], 'capture has unknown or missing fields');
  const snapshotUid = pattern(capture.source_snapshot_uid, UUID, 'capture snapshot uid is invalid');
  const captureLeaseId = pattern(capture.capture_lease_id, UUID, 'capture lease id is invalid');
  const sealedAt = timestamp(capture.sealed_at, 'capture sealed timestamp is invalid');
  const destroyedAt = timestamp(capture.plaintext_destroyed_at, 'capture destruction timestamp is invalid');
  if (destroyedAt < sealedAt || destroyedAt - sealedAt > 10 * 60 * 1000) fail('plaintext destruction window is invalid');
  boolean(capture.source_mount_read_only, true, 'source snapshot mount must be read-only');
  boolean(capture.plaintext_persistence, false, 'plaintext persistence must be disabled');
  boolean(capture.plaintext_logging, false, 'plaintext logging must be disabled');
  if (integer(capture.supplier_requests, 0, 0, 'capture supplier requests must be zero') !== 0 ||
      integer(capture.quota_reset_requests, 0, 0, 'capture reset requests must be zero') !== 0) fail('capture included forbidden provider activity');

  const packages = array(root.packages, 'packages must be an array').map(parsePackage);
  const targets = array(root.target_inventory, 'target inventory must be an array').map(parseTarget);
  const accountCount = integer(preflight.account_count, 1, 100, 'preflight account count is invalid');
  if (packages.length !== accountCount || targets.length !== packages.length) fail('capture and target cardinality does not match preflight');
  for (const selectedProvider of PROVIDERS) {
    const expected = integer(providerCounts[selectedProvider], 0, accountCount, `preflight ${selectedProvider} count is invalid`);
    if (packages.filter((item) => item.provider === selectedProvider).length !== expected) fail(`captured ${selectedProvider} count does not match preflight`);
  }
  const unique = (values: string[], code: string): void => { if (new Set(values).size !== values.length) fail(code); };
  unique(packages.map((item) => item.source_account_ref), 'source account references are not unique');
  unique(packages.map((item) => item.package_digest), 'sealed package digests are not unique');
  unique(packages.map((item) => item.target_account_id), 'target account ids are not unique');
  unique(targets.map((item) => item.target_account_id), 'target inventory account ids are not unique');
  for (const item of packages) {
    if (item.source_snapshot_uid !== snapshotUid || item.capture_lease_id !== captureLeaseId) fail('package is not bound to this capture');
  }

  const importFence = object(root.import_fence, 'import fence must be an object');
  exactKeys(importFence, [
    'mode', 'import_lease_id', 'single_writer', 'credential_generation_compare_and_swap',
    'state_generation_compare_and_swap',
    'expected_target_inventory_digest', 'activate_accounts', 'update_routes',
    'delete_source', 'supplier_requests', 'quota_reset_requests',
  ], 'import fence has unknown or missing fields');
  if (importFence.mode !== 'dry-run') fail('only dry-run mode is supported');
  pattern(importFence.import_lease_id, UUID, 'import lease id is invalid');
  boolean(importFence.single_writer, true, 'import must be single-writer');
  boolean(importFence.credential_generation_compare_and_swap, true, 'import credential generation CAS must be enabled');
  boolean(importFence.state_generation_compare_and_swap, true, 'import state generation CAS must be enabled');
  boolean(importFence.activate_accounts, false, 'dry-run cannot activate accounts');
  boolean(importFence.update_routes, false, 'dry-run cannot update routes');
  boolean(importFence.delete_source, false, 'dry-run cannot delete the source');
  if (integer(importFence.supplier_requests, 0, 0, 'import supplier requests must be zero') !== 0 ||
      integer(importFence.quota_reset_requests, 0, 0, 'import reset requests must be zero') !== 0) fail('import included forbidden provider activity');
  const expectedTargetDigest = pattern(importFence.expected_target_inventory_digest, SHA256, 'expected target inventory digest is invalid');
  if (expectedTargetDigest !== digest(targets, 'mtc.native-cli-target-inventory.v1')) fail('target inventory digest does not match');

  const rollback = object(root.rollback, 'rollback must be an object');
  exactKeys(rollback, ['source_workload_retained', 'source_pvc_retained', 'source_routes_retained', 'old_endpoint_retained', 'retirement_requires_user_approval'], 'rollback has unknown or missing fields');
  for (const key of ['source_workload_retained', 'source_pvc_retained', 'source_routes_retained', 'old_endpoint_retained', 'retirement_requires_user_approval'] as const) {
    boolean(rollback[key], true, `rollback ${key} must be enabled`);
  }

  const targetById = new Map(targets.map((item) => [item.target_account_id, item]));
  const actions: PlannedAction[] = packages.map((item) => {
    const target = targetById.get(item.target_account_id);
    if (target === undefined || target.target_tenant_id !== item.target_tenant_id || target.provider !== item.provider || target.driver !== item.target_driver || target.provider_label !== item.target_provider_label) {
      fail('sealed package target identity does not match target inventory');
    }
    let action: Action;
    if (target.status === 'absent') {
      if (item.target_credential_generation !== 1 || item.target_state_generation !== 0) {
        fail('new target generations must start at credential 1 and state 0');
      }
      action = 'create_disabled_account_and_install_state';
    } else {
      if (target.credential_generation !== item.target_credential_generation) fail('target credential generation changed');
      if (target.installed_package_digest === null) {
        if (item.target_state_generation !== 0) fail('first state installation must use generation 0');
        action = 'install_state';
      } else if (target.installed_package_digest === item.package_digest && target.state_generation === item.target_state_generation) action = 'no_op';
      else fail('target already contains a different package or state generation');
    }
    return {
      action,
      provider: item.provider,
      idempotency_key: digest({
        preflight: preflightEvidenceDigest,
        account: item.target_account_id,
        credential_generation: item.target_credential_generation,
        state_generation: item.target_state_generation,
        package: item.package_digest,
      }, 'mtc.native-cli-import-idempotency.v1'),
    };
  });
  return { packages, targets, actions, preflightEvidenceDigest };
}

const [mode = '', inputPath = ''] = process.argv.slice(2);
if (mode !== '--dry-run' || inputPath.length === 0 || process.argv.length !== 4) {
  fail('usage: provide --dry-run and exactly one sealed-capture JSON path; apply is intentionally unsupported');
}
let parsed: unknown;
try {
  parsed = JSON.parse(readFileSync(inputPath, 'utf8'));
} catch {
  fail('input is unavailable or invalid JSON');
}
const validated = validateDocument(parsed);
const providerCounts = Object.fromEntries(PROVIDERS.map((name) => [
  name,
  validated.packages.filter((item) => item.provider === name).length,
]));
const actionCounts = Object.fromEntries([
  'create_disabled_account_and_install_state', 'install_state', 'no_op',
].map((name) => [name, validated.actions.filter((item) => item.action === name).length]));
process.stdout.write(`${JSON.stringify({
  schema: 'mtc.native-cli-migration-import-dry-run.v1',
  status: 'dry_run_ready_for_owner_sealed_import',
  preflight_evidence_digest: validated.preflightEvidenceDigest,
  plan_digest: digest(validated.actions, 'mtc.native-cli-import-plan.v1'),
  target_inventory_digest: digest(validated.targets, 'mtc.native-cli-target-inventory.v1'),
  idempotency_set_digest: digest(validated.actions.map((item) => item.idempotency_key).sort(), 'mtc.native-cli-import-idempotency-set.v1'),
  account_count: validated.actions.length,
  provider_counts: providerCounts,
  action_counts: actionCounts,
  accounts_activated: 0,
  routes_updated: 0,
  source_resources_deleted: 0,
  supplier_requests: 0,
  quota_reset_requests: 0,
  next_required_receipts: [
    'owner_authorized_import_apply',
    'target_restore',
    'target_subject',
    'route_activation',
    'history_parity',
    'zero_legacy_fallback_window',
    'user_authorized_retirement',
  ],
})}\n`);
