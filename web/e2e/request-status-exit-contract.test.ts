import assert from 'node:assert/strict';
import test from 'node:test';
import { requestEgressLine, requestStatusCopy } from '../src/requestStatusPresentation.js';
import type { RequestEgressObservation, RequestView } from '../src/types.js';

// Synthetic fixtures only: documentation ranges (RFC 5737 / RFC 3849), no real
// user data, no credentials, no production addresses.
const request: RequestView = {
  usage_basis: 'provider_reported',
  request_id: 'fixture', created_at: 1, protocol: 'openai', model: 'fixture', status_code: 200,
  duration_ms: 2000, input_tokens: 160, cached_input_tokens: 40, cache_write_tokens: 20,
  output_tokens: 32, cost: '0', error_code: null,
};

const proxyObservation: RequestEgressObservation = {
  selected: {
    kind: 'proxy', group_id: 'fixture-group-internal-id', group_version: 3, group_name: '示例出口组',
    member_id: 'fixture-member-internal-id', member_name: 'edge-01',
    scheme: 'socks5h', host: '192.0.2.10', port: 1080,
  },
  observed_at: 1760000000000,
};

test('recorded proxy exit shows the human name and safe address, and hover carries group without internal IDs', () => {
  const line = requestEgressLine({ ...request, egress: proxyObservation }, 'zh-CN');
  assert.equal(line.kind, 'proxy');
  assert.equal(line.text, '出口: edge-01 · socks5h://192.0.2.10:1080');
  assert.match(line.hover, /示例出口组/);
  assert.match(line.hover, /组版本 3/);
  assert.match(line.hover, /edge-01/);
  assert.match(line.hover, /socks5h:\/\/192\.0\.2\.10:1080/);
  assert.match(line.hover, /连通性: 未记录/);
  assert.match(line.hover, /记录时间: \d/);
  assert.doesNotMatch(line.text + line.hover, /fixture-group-internal-id|fixture-member-internal-id/, 'internal IDs stay out of the visible copy');
  const english = requestEgressLine({ ...request, egress: proxyObservation }, 'en');
  assert.equal(english.text, 'Exit: edge-01 · socks5h://192.0.2.10:1080');
  assert.match(english.hover, /Connectivity: not recorded \(past use does not confirm current reachability\)/);
  assert.match(english.hover, /separate from the upstream account/, 'hover distinguishes the network proxy from the upstream account');
  assert.doesNotMatch(english.hover, /fixture-group-internal-id|fixture-member-internal-id/);
});

test('hostile snapshot addresses never surface userinfo, tokens or query text', () => {
  const hostile: RequestEgressObservation = {
    selected: {
      kind: 'proxy', group_id: null, group_version: null, group_name: null,
      member_id: null, member_name: null,
      scheme: 'socks5h', host: 'operator:Bearer-secret-token@203.0.113.9?session=attacker', port: 1080,
    },
    observed_at: null,
  };
  const line = requestEgressLine({ ...request, egress: hostile }, 'en');
  assert.doesNotMatch(line.text + line.hover, /Bearer|secret-token|session=attacker|@/);
  assert.match(line.hover, /Exit address: Not recorded/, 'an unusable address is explicitly unrecorded');
  assert.match(line.text, /Exit: Proxy exit/, 'unnamed exits fall back to the generic proxy label');
  const chinese = requestEgressLine({ ...request, egress: hostile }, 'zh-CN');
  assert.match(chinese.hover, /未归属代理组的出口/);
  assert.doesNotMatch(chinese.text + chinese.hover, /Bearer|secret-token|session=attacker/);
});

test('IPv6 exit addresses render bracketed without inventing data', () => {
  const line = requestEgressLine({
    ...request,
    egress: { selected: { ...proxyObservation.selected, host: '2001:db8::10', group_name: null, group_id: null, group_version: null }, observed_at: undefined },
  }, 'zh-CN');
  assert.match(line.text, /\[2001:db8::10\]:1080/);
  assert.match(line.hover, /记录时间: 未记录/);
  assert.match(line.hover, /未归属代理组的出口/);
});

test('direct exits say so plainly and never imply a proxy', () => {
  const direct: RequestEgressObservation = {
    selected: { kind: 'direct', group_id: null, group_version: null, group_name: null, member_id: null, member_name: null, scheme: null, host: null, port: null },
    observed_at: 1760000000000,
  };
  const line = requestEgressLine({ ...request, egress: direct }, 'zh-CN');
  assert.equal(line.kind, 'direct');
  assert.equal(line.text, '出口: 直连');
  assert.match(line.hover, /未使用代理出口/);
  assert.match(line.hover, /与上游账号不同/);
});

test('pre-admission terminal records state that no exit was ever selected', () => {
  const preAdmission = { ...request, status_code: 502, error_code: 'http_502', terminal_cause_code: 'gateway_pre_admission' };
  const line = requestEgressLine(preAdmission, 'zh-CN');
  assert.equal(line.kind, 'pre_admission');
  assert.equal(line.text, '尚未选择网络出口');
  assert.match(line.hover, /没有实际使用的出口/);
  const english = requestEgressLine(preAdmission, 'en');
  assert.equal(english.text, 'No network exit selected');
  assert.match(english.hover, /before a network exit was selected/);
  // A snapshot would be contradictory for a pre-admission record; the terminal cause wins.
  assert.equal(requestEgressLine({ ...preAdmission, egress: proxyObservation }, 'zh-CN').kind, 'pre_admission');
});

test('missing snapshots stay explicitly unrecorded for old and in-flight records', () => {
  const legacy = requestEgressLine(request, 'zh-CN');
  assert.equal(legacy.kind, 'unrecorded');
  assert.equal(legacy.text, '出口: 未记录');
  assert.match(legacy.hover, /早期请求可能未采集/);
  const pending = requestEgressLine({ ...request, status_code: null, duration_ms: null }, 'en');
  assert.equal(pending.kind, 'unrecorded');
  assert.match(pending.hover, /in-flight requests may not have returned one yet/);
  const explicitNull = requestEgressLine({ ...request, egress: null }, 'en');
  assert.equal(explicitNull.kind, 'unrecorded');
  const unknownKind = requestEgressLine({
    ...request,
    egress: { selected: { kind: 'unknown', group_id: null, group_version: null, group_name: null, member_id: null, member_name: null, scheme: null, host: null, port: null } },
  }, 'zh-CN');
  assert.equal(unknownKind.kind, 'unrecorded');
});

test('the exit line never changes the recorded status or failure cause', () => {
  const failed = { ...request, status_code: 502, error_code: 'upstream_transport_connection_reset', egress: proxyObservation };
  const status = requestStatusCopy(failed, 'zh-CN');
  assert.equal(status.label, '失败');
  assert.equal(status.hint, '已记录原因: 上游连接被重置');
  const line = requestEgressLine(failed, 'zh-CN');
  assert.equal(line.kind, 'proxy');
  assert.doesNotMatch(line.text, /失败|原因/, 'the exit line does not restate status boilerplate');
});
