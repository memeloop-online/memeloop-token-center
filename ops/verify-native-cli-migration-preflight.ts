import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';

const SCHEMA = 'mtc.native-cli-migration-preflight.v1';
const REVIEWED_SOURCE_IMAGE = 'sha256:1bc600741c7266a23a1567a2fc19b3708e0b4476eacbe9b5df1637e19d7c10e5';
const SHA256 = /^sha256:[0-9a-f]{64}$/u;
const GIT_REVISION = /^[0-9a-f]{40}$/u;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/u;
const RFC3339 = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/u;
const PROVIDERS = ['copilot', 'cursor'] as const;

type Provider = (typeof PROVIDERS)[number];
type JsonObject = Record<string, unknown>;

interface Inventory {
  source_account_ref: string;
  provider: Provider;
  pointer_record_digest: string;
  principal_digest: string;
  home_metadata_digest: string;
  auth_allowlist_digest: string;
  included_metadata_digest: string;
  home_file_count: number;
  included_file_count: number;
  excluded_file_count: number;
  home_total_bytes: number;
  included_total_bytes: number;
  directory_count: number;
  symlink_count: number;
  hardlink_count: number;
  special_file_count: number;
  changing_file_count: number;
  owner_uid: number;
  owner_gid: number;
}

interface Mapping {
  source_account_ref: string;
  provider: Provider;
  source_principal_digest: string;
  target_expected_principal_digest: string;
  target_tenant_id: string;
  target_account_id: string;
  target_provider_label: string;
  target_driver: string;
  target_credential_generation: number;
  target_state_generation: number;
  target_enabled: boolean;
  route_candidate_enabled: boolean;
  auth_allowlist_digest: string;
  activation_subject_check_required: boolean;
}

interface Request {
  capture: JsonObject;
  inventory_before: Inventory[];
  inventory_after: Inventory[];
  mappings: Mapping[];
  encryption: JsonObject;
  fence: JsonObject;
  rollback: JsonObject;
}

function fail(code: string): never {
  throw new Error(`native CLI migration preflight rejected: ${code}`);
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

function boolean(value: unknown, expected: boolean, code: string): void {
  if (value !== expected) fail(code);
}

function integer(value: unknown, minimum: number, maximum: number, code: string): number {
  if (!Number.isSafeInteger(value) || (value as number) < minimum || (value as number) > maximum) fail(code);
  return value as number;
}

function pattern(value: unknown, expected: RegExp, code: string): string {
  const parsed = string(value, code);
  if (!expected.test(parsed)) fail(code);
  return parsed;
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

function rejectSecretShapedData(value: unknown): void {
  const forbiddenKeys = new Set([
    'access_token', 'refresh_token', 'id_token', 'password', 'cookie',
    'authorization', 'client_secret', 'private_key', 'credential', 'credentials',
    'auth_json', 'state_archive', 'plaintext',
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

const inventoryKeys = [
  'source_account_ref', 'provider', 'pointer_record_digest', 'principal_digest',
  'home_metadata_digest', 'auth_allowlist_digest', 'included_metadata_digest',
  'home_file_count', 'included_file_count', 'excluded_file_count', 'home_total_bytes',
  'included_total_bytes', 'directory_count', 'symlink_count', 'hardlink_count',
  'special_file_count', 'changing_file_count', 'owner_uid', 'owner_gid',
] as const;

function parseInventory(value: unknown, index: number): Inventory {
  const item = object(value, `inventory ${index} must be an object`);
  exactKeys(item, inventoryKeys, `inventory ${index} has unknown or missing fields`);
  const homeFileCount = integer(item.home_file_count, 1, 10_000, `inventory ${index} file count is invalid`);
  const includedFileCount = integer(item.included_file_count, 1, homeFileCount, `inventory ${index} included file count is invalid`);
  const excludedFileCount = integer(item.excluded_file_count, 0, homeFileCount, `inventory ${index} excluded file count is invalid`);
  if (includedFileCount + excludedFileCount !== homeFileCount) fail(`inventory ${index} is not completely classified`);
  const homeTotalBytes = integer(item.home_total_bytes, 1, 512 * 1024 * 1024, `inventory ${index} byte count is invalid`);
  const includedTotalBytes = integer(item.included_total_bytes, 1, homeTotalBytes, `inventory ${index} included bytes are invalid`);
  for (const key of ['symlink_count', 'hardlink_count', 'special_file_count', 'changing_file_count'] as const) {
    if (integer(item[key], 0, 10_000, `inventory ${index} unsafe entry count is invalid`) !== 0) {
      fail(`inventory ${index} contains unsafe or changing entries`);
    }
  }
  return {
    source_account_ref: pattern(item.source_account_ref, SHA256, `inventory ${index} account reference is invalid`),
    provider: provider(item.provider, `inventory ${index} provider is invalid`),
    pointer_record_digest: pattern(item.pointer_record_digest, SHA256, `inventory ${index} pointer digest is invalid`),
    principal_digest: pattern(item.principal_digest, SHA256, `inventory ${index} principal digest is invalid`),
    home_metadata_digest: pattern(item.home_metadata_digest, SHA256, `inventory ${index} metadata digest is invalid`),
    auth_allowlist_digest: pattern(item.auth_allowlist_digest, SHA256, `inventory ${index} allowlist digest is invalid`),
    included_metadata_digest: pattern(item.included_metadata_digest, SHA256, `inventory ${index} included metadata digest is invalid`),
    home_file_count: homeFileCount,
    included_file_count: includedFileCount,
    excluded_file_count: excludedFileCount,
    home_total_bytes: homeTotalBytes,
    included_total_bytes: includedTotalBytes,
    directory_count: integer(item.directory_count, 1, 10_000, `inventory ${index} directory count is invalid`),
    symlink_count: 0,
    hardlink_count: 0,
    special_file_count: 0,
    changing_file_count: 0,
    owner_uid: integer(item.owner_uid, 1, 2 ** 31 - 1, `inventory ${index} owner uid is invalid`),
    owner_gid: integer(item.owner_gid, 1, 2 ** 31 - 1, `inventory ${index} owner gid is invalid`),
  };
}

const mappingKeys = [
  'source_account_ref', 'provider', 'source_principal_digest', 'target_expected_principal_digest',
  'target_tenant_id', 'target_account_id', 'target_provider_label', 'target_driver',
  'target_credential_generation', 'target_state_generation', 'target_enabled',
  'route_candidate_enabled', 'auth_allowlist_digest',
  'activation_subject_check_required',
] as const;

function parseMapping(value: unknown, index: number): Mapping {
  const item = object(value, `mapping ${index} must be an object`);
  exactKeys(item, mappingKeys, `mapping ${index} has unknown or missing fields`);
  const selectedProvider = provider(item.provider, `mapping ${index} provider is invalid`);
  const expectedLabel = selectedProvider === 'copilot' ? 'Copilot' : 'Cursor';
  if (item.target_provider_label !== expectedLabel) fail(`mapping ${index} target label is not native`);
  if (item.target_driver !== `${selectedProvider}-cli`) fail(`mapping ${index} target driver is not native`);
  boolean(item.target_enabled, false, `mapping ${index} target must remain disabled`);
  boolean(item.route_candidate_enabled, false, `mapping ${index} route candidate must remain disabled`);
  boolean(item.activation_subject_check_required, true, `mapping ${index} must require an activation subject check`);
  const sourcePrincipal = pattern(item.source_principal_digest, SHA256, `mapping ${index} source principal is invalid`);
  const targetPrincipal = pattern(item.target_expected_principal_digest, SHA256, `mapping ${index} target principal is invalid`);
  if (sourcePrincipal !== targetPrincipal) fail(`mapping ${index} principal identity does not match`);
  return {
    source_account_ref: pattern(item.source_account_ref, SHA256, `mapping ${index} account reference is invalid`),
    provider: selectedProvider,
    source_principal_digest: sourcePrincipal,
    target_expected_principal_digest: targetPrincipal,
    target_tenant_id: tenantId(item.target_tenant_id, `mapping ${index} tenant id is invalid`),
    target_account_id: pattern(item.target_account_id, UUID, `mapping ${index} account id is invalid`),
    target_provider_label: expectedLabel,
    target_driver: `${selectedProvider}-cli`,
    target_credential_generation: integer(item.target_credential_generation, 1, 2 ** 31 - 1, `mapping ${index} credential generation is invalid`),
    target_state_generation: integer(item.target_state_generation, 0, 2 ** 31 - 1, `mapping ${index} state generation is invalid`),
    target_enabled: false,
    route_candidate_enabled: false,
    auth_allowlist_digest: pattern(item.auth_allowlist_digest, SHA256, `mapping ${index} allowlist digest is invalid`),
    activation_subject_check_required: true,
  };
}

function stableJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(',')}]`;
  if (value !== null && typeof value === 'object') {
    return `{${Object.entries(value as JsonObject).sort(([left], [right]) => left.localeCompare(right))
      .map(([key, child]) => `${JSON.stringify(key)}:${stableJson(child)}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

function validateDocument(value: unknown): Request {
  rejectSecretShapedData(value);
  const root = object(value, 'document must be an object');
  exactKeys(root, ['schema', 'capture', 'inventory_before', 'inventory_after', 'mappings', 'encryption', 'fence', 'rollback'], 'document has unknown or missing fields');
  if (root.schema !== SCHEMA) fail('schema is unsupported');

  const capture = object(root.capture, 'capture must be an object');
  exactKeys(capture, [
    'mode', 'source_namespace', 'source_workload_uid', 'source_pvc_name', 'source_pvc_uid',
    'source_snapshot_uid', 'source_snapshot_ready', 'source_mount_read_only',
    'gitops_revision', 'source_image_digest', 'started_at', 'completed_at',
    'supplier_requests', 'quota_reset_requests',
  ], 'capture has unknown or missing fields');
  if (capture.mode !== 'metadata-only') fail('capture mode must be metadata-only');
  if (capture.source_namespace !== 'cliproxyapi') fail('source namespace is not the reviewed source');
  if (capture.source_pvc_name !== 'cpa-copilot-cursor-data') fail('source PVC is not the reviewed source');
  pattern(capture.source_workload_uid, UUID, 'source workload uid is invalid');
  pattern(capture.source_pvc_uid, UUID, 'source PVC uid is invalid');
  pattern(capture.source_snapshot_uid, UUID, 'source snapshot uid is invalid');
  pattern(capture.gitops_revision, GIT_REVISION, 'GitOps revision is invalid');
  if (pattern(capture.source_image_digest, SHA256, 'source image digest is invalid') !== REVIEWED_SOURCE_IMAGE) {
    fail('source image digest has not been reviewed');
  }
  boolean(capture.source_snapshot_ready, true, 'source snapshot is not ready');
  boolean(capture.source_mount_read_only, true, 'source snapshot mount is not read-only');
  if (integer(capture.supplier_requests, 0, 0, 'supplier requests must be zero') !== 0) fail('supplier requests must be zero');
  if (integer(capture.quota_reset_requests, 0, 0, 'quota reset requests must be zero') !== 0) fail('quota reset requests must be zero');
  const started = timestamp(capture.started_at, 'capture start timestamp is invalid');
  const completed = timestamp(capture.completed_at, 'capture completion timestamp is invalid');
  if (completed < started || completed - started > 60 * 60 * 1000) fail('capture time window is invalid');

  const before = array(root.inventory_before, 'inventory_before must be an array').map(parseInventory);
  const after = array(root.inventory_after, 'inventory_after must be an array').map(parseInventory);
  const mappings = array(root.mappings, 'mappings must be an array').map(parseMapping);
  if (before.length === 0 || before.length > 100 || before.length !== after.length || before.length !== mappings.length) {
    fail('inventory and mapping cardinality does not match');
  }
  const unique = (values: string[], code: string): void => {
    if (new Set(values).size !== values.length) fail(code);
  };
  unique(before.map((item) => item.source_account_ref), 'source account references are not unique');
  unique(before.map((item) => item.pointer_record_digest), 'source pointer records are not unique');
  unique(before.map((item) => `${item.provider}:${item.principal_digest}`), 'source provider principals are not unique');
  unique(after.map((item) => item.source_account_ref), 'after-snapshot account references are not unique');
  unique(mappings.map((item) => item.source_account_ref), 'mapping source references are not unique');
  unique(mappings.map((item) => item.target_account_id), 'target account ids are not unique');

  const afterBySource = new Map(after.map((item) => [item.source_account_ref, item]));
  const mappingBySource = new Map(mappings.map((item) => [item.source_account_ref, item]));
  for (const source of before) {
    const final = afterBySource.get(source.source_account_ref);
    const mapping = mappingBySource.get(source.source_account_ref);
    if (final === undefined || stableJson(final) !== stableJson(source)) fail('source inventory changed during preflight');
    if (mapping === undefined || mapping.provider !== source.provider) fail('source provider mapping does not match');
    if (mapping.source_principal_digest !== source.principal_digest) fail('source principal mapping does not match');
    if (mapping.auth_allowlist_digest !== source.auth_allowlist_digest) fail('source allowlist mapping does not match');
  }

  const encryption = object(root.encryption, 'encryption must be an object');
  exactKeys(encryption, [
    'envelope', 'key_reference_digest', 'aad_fields', 'plaintext_persistence',
    'plaintext_logging', 'sealed_capture_required',
  ], 'encryption has unknown or missing fields');
  if (encryption.envelope !== 'mtc-upstream-credential-v2') fail('encryption envelope is unsupported');
  pattern(encryption.key_reference_digest, SHA256, 'encryption key reference digest is invalid');
  const aad = array(encryption.aad_fields, 'encryption aad fields must be an array');
  if (stableJson(aad) !== stableJson(['tenant_external_id', 'account_id', 'provider', 'state_generation'])) fail('encryption aad binding is incomplete');
  boolean(encryption.plaintext_persistence, false, 'plaintext persistence must be disabled');
  boolean(encryption.plaintext_logging, false, 'plaintext logging must be disabled');
  boolean(encryption.sealed_capture_required, true, 'sealed capture must be required');

  const fence = object(root.fence, 'fence must be an object');
  exactKeys(fence, [
    'capture_lease_id', 'single_writer', 'source_snapshot_frozen',
    'target_credential_generation_compare_and_swap',
    'target_state_generation_compare_and_swap', 'activation_requires_restore_receipt',
    'activation_requires_subject_receipt', 'activation_requires_route_receipt',
  ], 'fence has unknown or missing fields');
  pattern(fence.capture_lease_id, UUID, 'capture lease id is invalid');
  for (const key of [
    'single_writer', 'source_snapshot_frozen',
    'target_credential_generation_compare_and_swap',
    'target_state_generation_compare_and_swap',
    'activation_requires_restore_receipt', 'activation_requires_subject_receipt',
    'activation_requires_route_receipt',
  ] as const) boolean(fence[key], true, `fence ${key} must be enabled`);

  const rollback = object(root.rollback, 'rollback must be an object');
  exactKeys(rollback, [
    'source_workload_retained', 'source_pvc_retained', 'source_routes_retained',
    'old_endpoint_retained', 'rollback_image_digest', 'retire_after_state_receipt',
    'retire_after_history_receipt', 'retire_after_zero_fallback_window',
  ], 'rollback has unknown or missing fields');
  for (const key of [
    'source_workload_retained', 'source_pvc_retained', 'source_routes_retained',
    'old_endpoint_retained', 'retire_after_state_receipt', 'retire_after_history_receipt',
    'retire_after_zero_fallback_window',
  ] as const) boolean(rollback[key], true, `rollback ${key} must be enabled`);
  pattern(rollback.rollback_image_digest, SHA256, 'rollback image digest is invalid');

  return { capture, inventory_before: before, inventory_after: after, mappings, encryption, fence, rollback };
}

const [inputPath = ''] = process.argv.slice(2);
if (inputPath.length === 0 || process.argv.length !== 3) fail('usage: provide exactly one preflight JSON path');
let parsed: unknown;
try {
  parsed = JSON.parse(readFileSync(inputPath, 'utf8'));
} catch {
  fail('input is unavailable or invalid JSON');
}
const validated = validateDocument(parsed);
const providers = Object.fromEntries(PROVIDERS.map((name) => [
  name,
  validated.mappings.filter((mapping) => mapping.provider === name).length,
]));
const evidenceDigest = `sha256:${createHash('sha256').update(stableJson(parsed)).digest('hex')}`;
process.stdout.write(`${JSON.stringify({
  schema: 'mtc.native-cli-migration-preflight-receipt.v1',
  status: 'preflight_ready_for_sealed_capture',
  evidence_digest: evidenceDigest,
  account_count: validated.mappings.length,
  provider_counts: providers,
  supplier_requests: 0,
  quota_reset_requests: 0,
  next_required_receipts: [
    'sealed_capture',
    'target_restore',
    'target_subject',
    'route_activation',
    'history_parity',
    'zero_legacy_fallback_window',
  ],
})}\n`);
