import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { repository } from './contract-helpers.ts';

const digest = (character: string): string => `sha256:${character.repeat(64)}`;
const id = (tail: string): string => `018f47d2-7b8a-7abc-8def-${tail.padStart(12, '0')}`;

function inventory(provider: 'copilot' | 'cursor', account: string, character: string) {
  return {
    source_account_ref: account,
    provider,
    pointer_record_digest: digest(character),
    principal_digest: digest(provider === 'copilot' ? '3' : '4'),
    home_metadata_digest: digest('5'),
    auth_allowlist_digest: digest(provider === 'copilot' ? '6' : '7'),
    included_metadata_digest: digest('8'),
    home_file_count: 5,
    included_file_count: 2,
    excluded_file_count: 3,
    home_total_bytes: 8192,
    included_total_bytes: 4096,
    directory_count: 4,
    symlink_count: 0,
    hardlink_count: 0,
    special_file_count: 0,
    changing_file_count: 0,
    owner_uid: 10001,
    owner_gid: 10001,
  };
}

function fixture() {
  const copilot = inventory('copilot', digest('1'), 'a');
  const cursor = inventory('cursor', digest('2'), 'b');
  return {
    schema: 'mtc.native-cli-migration-preflight.v1',
    capture: {
      mode: 'metadata-only',
      source_namespace: 'cliproxyapi',
      source_workload_uid: id('1'),
      source_pvc_name: 'cpa-copilot-cursor-data',
      source_pvc_uid: id('2'),
      source_snapshot_uid: id('3'),
      source_snapshot_ready: true,
      source_mount_read_only: true,
      gitops_revision: '9'.repeat(40),
      source_image_digest: 'sha256:1bc600741c7266a23a1567a2fc19b3708e0b4476eacbe9b5df1637e19d7c10e5',
      started_at: '2026-09-11T00:00:00Z',
      completed_at: '2026-09-11T00:05:00Z',
      supplier_requests: 0,
      quota_reset_requests: 0,
    },
    inventory_before: [copilot, cursor],
    inventory_after: [structuredClone(copilot), structuredClone(cursor)],
    mappings: [copilot, cursor].map((source, index) => ({
      source_account_ref: source.source_account_ref,
      provider: source.provider,
      source_principal_digest: source.principal_digest,
      target_expected_principal_digest: source.principal_digest,
      target_tenant_id: id('4'),
      target_account_id: id(String(index + 5)),
      target_provider_label: source.provider === 'copilot' ? 'Copilot' : 'Cursor',
      target_driver: `${source.provider}-cli`,
      target_credential_generation: 1,
      target_state_generation: 0,
      target_enabled: false,
      route_candidate_enabled: false,
      auth_allowlist_digest: source.auth_allowlist_digest,
      activation_subject_check_required: true,
    })),
    encryption: {
      envelope: 'mtc-upstream-credential-v2',
      key_reference_digest: digest('c'),
      aad_fields: ['tenant_external_id', 'account_id', 'provider', 'state_generation'],
      plaintext_persistence: false,
      plaintext_logging: false,
      sealed_capture_required: true,
    },
    fence: {
      capture_lease_id: id('7'),
      single_writer: true,
      source_snapshot_frozen: true,
      target_credential_generation_compare_and_swap: true,
      target_state_generation_compare_and_swap: true,
      activation_requires_restore_receipt: true,
      activation_requires_subject_receipt: true,
      activation_requires_route_receipt: true,
    },
    rollback: {
      source_workload_retained: true,
      source_pvc_retained: true,
      source_routes_retained: true,
      old_endpoint_retained: true,
      rollback_image_digest: digest('d'),
      retire_after_state_receipt: true,
      retire_after_history_receipt: true,
      retire_after_zero_fallback_window: true,
    },
  };
}

function execute(document: unknown) {
  const directory = mkdtempSync(resolve(tmpdir(), 'native-cli-preflight-'));
  const input = resolve(directory, 'input.json');
  writeFileSync(input, JSON.stringify(document), { mode: 0o600 });
  return spawnSync(process.execPath, ['ops/verify-native-cli-migration-preflight.ts', input], {
    cwd: repository,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

test('accepts a stable metadata-only, disabled and fully fenced migration plan', () => {
  const result = execute(fixture());
  assert.equal(result.status, 0, result.stderr);
  const receipt = JSON.parse(result.stdout);
  assert.equal(receipt.status, 'preflight_ready_for_sealed_capture');
  assert.equal(receipt.account_count, 2);
  assert.deepEqual(receipt.provider_counts, { copilot: 1, cursor: 1 });
  assert.equal(receipt.supplier_requests, 0);
  assert.equal(receipt.quota_reset_requests, 0);
  assert.match(receipt.evidence_digest, /^sha256:[0-9a-f]{64}$/u);
  assert.ok(!result.stdout.includes('source_account_ref'));
  assert.ok(!result.stdout.includes('target_account_id'));
});

test('rejects mutable, linked, incompletely classified or identity-mismatched state', () => {
  const cases = [
    (value: ReturnType<typeof fixture>) => { value.inventory_after[0]!.home_total_bytes += 1; },
    (value: ReturnType<typeof fixture>) => { value.inventory_before[0]!.symlink_count = 1; value.inventory_after[0]!.symlink_count = 1; },
    (value: ReturnType<typeof fixture>) => { value.inventory_before[0]!.excluded_file_count = 2; value.inventory_after[0]!.excluded_file_count = 2; },
    (value: ReturnType<typeof fixture>) => { value.mappings[0]!.target_expected_principal_digest = digest('f'); },
  ];
  for (const mutate of cases) {
    const value = fixture();
    mutate(value);
    assert.notEqual(execute(value).status, 0);
  }
});

test('rejects activation, unreviewed native naming, supplier traffic and reset activity', () => {
  const cases = [
    (value: ReturnType<typeof fixture>) => { value.mappings[0]!.target_enabled = true; },
    (value: ReturnType<typeof fixture>) => { value.mappings[0]!.target_provider_label = 'cpa-Copilot'; },
    (value: ReturnType<typeof fixture>) => { value.mappings[0]!.target_driver = 'bridge'; },
    (value: ReturnType<typeof fixture>) => { value.capture.supplier_requests = 1; },
    (value: ReturnType<typeof fixture>) => { value.capture.quota_reset_requests = 1; },
    (value: ReturnType<typeof fixture>) => { value.rollback.source_pvc_retained = false; },
  ];
  for (const mutate of cases) {
    const value = fixture();
    mutate(value);
    assert.notEqual(execute(value).status, 0);
  }
});

test('rejects secret-bearing input without echoing its value', () => {
  const value = fixture() as ReturnType<typeof fixture> & { access_token?: string };
  value.access_token = 'github_pat_DO_NOT_ECHO_012345678901234567890';
  const result = execute(value);
  assert.notEqual(result.status, 0);
  assert.ok(!result.stderr.includes(value.access_token));
  assert.ok(!result.stdout.includes(value.access_token));
});

test('implementation never traverses a source state directory or invokes a supplier', () => {
  const source = readFileSync(resolve(repository, 'ops/verify-native-cli-migration-preflight.ts'), 'utf8');
  assert.ok(!source.includes('readdirSync'));
  assert.ok(!source.includes('fetch('));
  assert.ok(!source.includes('spawn'));
  assert.ok(!source.includes('kubectl'));
  assert.ok(!source.includes('/v1/'));
});
