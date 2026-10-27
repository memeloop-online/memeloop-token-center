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
  assert.ok(line);
  assert.equal(line.kind, 'proxy');
  assert.equal(line.text, 'edge-01 · socks5h://192.0.2.10:1080');
  assert.match(line.hover, /示例出口组/);
  assert.match(line.hover, /组版本 3/);
  assert.match(line.hover, /edge-01/);
  assert.match(line.hover, /socks5h:\/\/192\.0\.2\.10:1080/);
  assert.match(line.hover, /连通性/);
  assert.match(line.hover, /记录时间: \d/);
  assert.doesNotMatch(line.text + line.hover, /fixture-group-internal-id|fixture-member-internal-id/, 'internal IDs stay out of the visible copy');
  const english = requestEgressLine({ ...request, egress: proxyObservation }, 'en');
  assert.ok(english);
  assert.equal(english.text, 'edge-01 · socks5h://192.0.2.10:1080');
  assert.match(english.hover, /Connectivity/);
  assert.match(english.hover, /This request used a proxy exit/);
  assert.doesNotMatch(english.hover, /fixture-group-internal-id|fixture-member-internal-id/);
});

test('hostile snapshot addresses never surface userinfo, tokens or query text', () => {
  const hostile: RequestEgressObservation = {
    selected: {
      kind: 'proxy', group_id: null, group_version: null, group_name: null,
      member_id: null, member_name: 'edge-09',
      scheme: 'socks5h', host: 'operator:Bearer-secret-token@203.0.113.9?session=attacker', port: 1080,
    },
    observed_at: null,
  };
  const line = requestEgressLine({ ...request, egress: hostile }, 'en');
  assert.ok(line);
  assert.doesNotMatch(line.text + line.hover, /Bearer|secret-token|session=attacker|@/);
  assert.match(line.hover, /Exit address: Not recorded/, 'an unusable address is explicitly unrecorded');
  assert.equal(line.text, 'edge-09', 'a named exit with an unusable address shows only the real name');
  const chinese = requestEgressLine({ ...request, egress: hostile }, 'zh-CN');
  assert.ok(chinese);
  assert.equal(chinese.text, 'edge-09');
  assert.match(chinese.hover, /未归属代理组的出口/);
  assert.doesNotMatch(chinese.text + chinese.hover, /Bearer|secret-token|session=attacker/);
});

test('a proxy snapshot with neither a name nor a usable address renders no exit line', () => {
  const nameless: RequestEgressObservation = {
    selected: {
      kind: 'proxy', group_id: null, group_version: null, group_name: null,
      member_id: null, member_name: null,
      scheme: 'socks5h', host: 'operator:Bearer-secret-token@203.0.113.9?session=attacker', port: 1080,
    },
    observed_at: null,
  };
  assert.equal(requestEgressLine({ ...request, egress: nameless }, 'zh-CN'), null);
  assert.equal(requestEgressLine({ ...request, egress: nameless }, 'en'), null);
});

test('IPv6 exit addresses render bracketed without inventing data', () => {
  const line = requestEgressLine({
    ...request,
    egress: { selected: { ...proxyObservation.selected, host: '2001:db8::10', group_name: null, group_id: null, group_version: null }, observed_at: undefined },
  }, 'zh-CN');
  assert.ok(line);
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
  assert.ok(line);
  assert.equal(line.kind, 'direct');
  assert.equal(line.text, '直连');
  assert.match(line.hover, /这次请求走直连/);
  assert.doesNotMatch(line.hover, /不是|不会|不代表|与上游账号不同/);
  const english = requestEgressLine({ ...request, egress: direct }, 'en');
  assert.ok(english);
  assert.equal(english.text, 'Direct');
});

test('pre-admission terminal records render no exit line, keeping the evidence in the status cause', () => {
  const preAdmission = { ...request, status_code: 502, error_code: 'http_502', terminal_cause_code: 'gateway_pre_admission' };
  assert.equal(requestEgressLine(preAdmission, 'zh-CN'), null);
  assert.equal(requestEgressLine(preAdmission, 'en'), null);
  // A snapshot would be contradictory for a pre-admission record; the terminal cause wins.
  assert.equal(requestEgressLine({ ...preAdmission, egress: proxyObservation }, 'zh-CN'), null);
  assert.match(requestStatusCopy(preAdmission, 'zh-CN').cause, /请求没有发到上游/);
  assert.match(requestStatusCopy(preAdmission, 'en').cause, /was not sent upstream/);
});

test('missing snapshots render no exit line for old, in-flight, explicit-null and unknown-kind records', () => {
  assert.equal(requestEgressLine(request, 'zh-CN'), null);
  assert.equal(requestEgressLine(request, 'en'), null);
  const pending = requestEgressLine({ ...request, status_code: null, duration_ms: null }, 'en');
  assert.equal(pending, null);
  const explicitNull = requestEgressLine({ ...request, egress: null }, 'zh-CN');
  assert.equal(explicitNull, null);
  const unknownKind = requestEgressLine({
    ...request,
    egress: { selected: { kind: 'unknown', group_id: null, group_version: null, group_name: null, member_id: null, member_name: null, scheme: null, host: null, port: null } },
  }, 'en');
  assert.equal(unknownKind, null);
});

test('the exit line never changes the recorded status or failure cause', () => {
  const failed = { ...request, status_code: 502, error_code: 'upstream_transport_connection_reset', egress: proxyObservation };
  const status = requestStatusCopy(failed, 'zh-CN');
  assert.equal(status.label, '失败');
  assert.equal(status.hint, '已记录原因: 上游连接被重置');
  const line = requestEgressLine(failed, 'zh-CN');
  assert.ok(line);
  assert.equal(line.kind, 'proxy');
  assert.doesNotMatch(line.text, /失败|原因/, 'the exit line does not restate status boilerplate');
});
