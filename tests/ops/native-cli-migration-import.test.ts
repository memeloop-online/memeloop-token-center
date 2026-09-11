import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { repository } from './contract-helpers.ts';

type JsonObject = Record<string, unknown>;
interface TargetFixture {
  target_tenant_id: string;
  target_account_id: string;
  provider: 'copilot' | 'cursor';
  provider_label: string;
  driver: string;
  status: 'absent' | 'disabled';
  credential_generation: number;
  state_generation: number | null;
  installed_package_digest: string | null;
  route_candidate_count: number;
}

const digest = (character: string): string => `sha256:${character.repeat(64)}`;
const id = (tail: string): string => `018f47d2-7b8a-7abc-8def-${tail.padStart(12, '0')}`;

function stableJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(',')}]`;
  if (value !== null && typeof value === 'object') {
    return `{${Object.entries(value as JsonObject).sort(([left], [right]) => left.localeCompare(right))
      .map(([key, child]) => `${JSON.stringify(key)}:${stableJson(child)}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

function domainDigest(value: unknown, domain: string): string {
  return `sha256:${createHash('sha256').update(domain).update('\0').update(stableJson(value)).digest('hex')}`;
}

function fixture() {
  const snapshot = id('3');
  const lease = id('7');
  const packages = (['copilot', 'cursor'] as const).map((provider, index) => ({
    source_account_ref: digest(index === 0 ? '1' : '2'),
    provider,
    target_tenant_id: id('4'),
    target_account_id: id(String(index + 5)),
    target_credential_generation: 1,
    target_state_generation: 0,
    target_provider_label: provider === 'copilot' ? 'Copilot' : 'Cursor',
    target_driver: `${provider}-cli`,
    auth_allowlist_digest: digest(index === 0 ? '6' : '7'),
    source_snapshot_uid: snapshot,
    capture_lease_id: lease,
    envelope: 'mtc-upstream-credential-v2',
    algorithm: 'chacha20-poly1305',
    key_reference_digest: digest('c'),
    aad_fields: ['tenant_external_id', 'account_id', 'provider', 'state_generation'],
    aad_digest: domainDigest({
      tenant_external_id: id('4'),
      account_id: id(String(index + 5)),
      provider,
      state_generation: 0,
    }, 'memeloop-token-center/native-cli-state/v1'),
    package_digest: digest(index === 0 ? 'a' : 'b'),
    plaintext_file_count: 2,
    plaintext_bytes: 4096,
    ciphertext_bytes: 4352,
  }));
  const targetInventory: TargetFixture[] = packages.map((item) => ({
    target_tenant_id: item.target_tenant_id,
    target_account_id: item.target_account_id,
    provider: item.provider,
    provider_label: item.target_provider_label,
    driver: item.target_driver,
    status: 'absent',
    credential_generation: 0,
    state_generation: null,
    installed_package_digest: null,
    route_candidate_count: 0,
  }));
  return {
    schema: 'mtc.native-cli-sealed-capture.v1',
    preflight_receipt: {
      schema: 'mtc.native-cli-migration-preflight-receipt.v1',
      status: 'preflight_ready_for_sealed_capture',
      evidence_digest: digest('e'),
      account_count: 2,
      provider_counts: { copilot: 1, cursor: 1 },
      supplier_requests: 0,
      quota_reset_requests: 0,
    },
    capture: {
      source_snapshot_uid: snapshot,
      capture_lease_id: lease,
      sealed_at: '2026-09-11T00:10:00Z',
      plaintext_destroyed_at: '2026-09-11T00:10:30Z',
      source_mount_read_only: true,
      plaintext_persistence: false,
      plaintext_logging: false,
      supplier_requests: 0,
      quota_reset_requests: 0,
    },
    packages,
    target_inventory: targetInventory,
    import_fence: {
      mode: 'dry-run',
      import_lease_id: id('8'),
      single_writer: true,
      credential_generation_compare_and_swap: true,
      state_generation_compare_and_swap: true,
      expected_target_inventory_digest: domainDigest(targetInventory, 'mtc.native-cli-target-inventory.v1'),
      activate_accounts: false,
      update_routes: false,
      delete_source: false,
      supplier_requests: 0,
      quota_reset_requests: 0,
    },
    rollback: {
      source_workload_retained: true,
      source_pvc_retained: true,
      source_routes_retained: true,
      old_endpoint_retained: true,
      retirement_requires_user_approval: true,
    },
  };
}

function execute(document: unknown, args: string[] = ['--dry-run']) {
  const directory = mkdtempSync(resolve(tmpdir(), 'native-cli-import-'));
  const input = resolve(directory, 'input.json');
  writeFileSync(input, JSON.stringify(document), { mode: 0o600 });
  return spawnSync(process.execPath, ['ops/plan-native-cli-migration-import.ts', ...args, input], {
    cwd: repository,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
}

test('builds a deterministic, disabled and provider-silent import dry-run', () => {
  const first = execute(fixture());
  const second = execute(fixture());
  assert.equal(first.status, 0, first.stderr);
  assert.equal(second.status, 0, second.stderr);
  assert.equal(first.stdout, second.stdout);
  const receipt = JSON.parse(first.stdout);
  assert.equal(receipt.status, 'dry_run_ready_for_owner_sealed_import');
  assert.deepEqual(receipt.provider_counts, { copilot: 1, cursor: 1 });
  assert.deepEqual(receipt.action_counts, {
    create_disabled_account_and_install_state: 2,
    install_state: 0,
    no_op: 0,
  });
  assert.equal(receipt.accounts_activated, 0);
  assert.equal(receipt.routes_updated, 0);
  assert.equal(receipt.supplier_requests, 0);
  assert.equal(receipt.quota_reset_requests, 0);
  assert.ok(!first.stdout.includes(id('5')));
  assert.ok(!first.stdout.includes(digest('1')));
});

test('turns an exact previously installed package into an idempotent no-op', () => {
  const value = fixture();
  for (let index = 0; index < value.target_inventory.length; index += 1) {
    const target = value.target_inventory[index]!;
    target.status = 'disabled';
    target.credential_generation = 1;
    target.state_generation = 0;
    target.installed_package_digest = value.packages[index]!.package_digest;
  }
  value.import_fence.expected_target_inventory_digest = domainDigest(value.target_inventory, 'mtc.native-cli-target-inventory.v1');
  const result = execute(value);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).action_counts.no_op, 2);
});

test('rejects an enabled, routed, renamed, stale or conflicting target', () => {
  const cases = [
    (value: ReturnType<typeof fixture>) => { value.target_inventory[0]!.status = 'active' as 'absent'; },
    (value: ReturnType<typeof fixture>) => { value.target_inventory[0]!.route_candidate_count = 1; },
    (value: ReturnType<typeof fixture>) => { value.packages[0]!.target_provider_label = 'cpa-Copilot'; },
    (value: ReturnType<typeof fixture>) => { value.packages[0]!.target_driver = 'bridge'; },
    (value: ReturnType<typeof fixture>) => { value.packages[0]!.target_state_generation = 1; },
    (value: ReturnType<typeof fixture>) => { value.packages[0]!.aad_digest = digest('f'); },
    (value: ReturnType<typeof fixture>) => { value.import_fence.expected_target_inventory_digest = digest('f'); },
    (value: ReturnType<typeof fixture>) => {
      const target = value.target_inventory[0]!;
      target.status = 'disabled';
      target.credential_generation = 1;
      target.state_generation = 1;
      target.installed_package_digest = digest('f');
    },
  ];
  for (const mutate of cases) {
    const value = fixture();
    mutate(value);
    if (value.import_fence.expected_target_inventory_digest !== digest('f')) {
      value.import_fence.expected_target_inventory_digest = domainDigest(value.target_inventory, 'mtc.native-cli-target-inventory.v1');
    }
    assert.notEqual(execute(value).status, 0);
  }
});

test('rejects apply mode and secret-bearing input without echoing the value', () => {
  assert.notEqual(execute(fixture(), ['--apply']).status, 0);
  const value = fixture() as ReturnType<typeof fixture> & { access_token?: string };
  value.access_token = 'github_pat_DO_NOT_ECHO_012345678901234567890';
  const result = execute(value);
  assert.notEqual(result.status, 0);
  assert.ok(!result.stderr.includes(value.access_token));
  assert.ok(!result.stdout.includes(value.access_token));
});

test('dry-run implementation cannot inspect state files, invoke a supplier or mutate a cluster', () => {
  const source = readFileSync(resolve(repository, 'ops/plan-native-cli-migration-import.ts'), 'utf8');
  assert.ok(!source.includes('readdirSync'));
  assert.ok(!source.includes('fetch('));
  assert.ok(!source.includes('spawn'));
  assert.ok(!source.includes('kubectl'));
  assert.ok(!source.includes('writeFile'));
});
