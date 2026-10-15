import assert from 'node:assert/strict';
import test from 'node:test';
import { egressAddress, egressBytes, egressGroupKey, egressSelectionKey, egressSelectionName, egressSummaryPath } from '../src/operator/requestEgress.js';
import { egressSummaries, historicalExit } from './support/request-egress-fixtures.js';

test('egress preserves directional unknown and zero, historical snapshots, names and safe address components', () => {
  for (const locale of ['zh-CN', 'en']) {
    assert.notEqual(egressBytes(null, locale), egressBytes(0, locale));
    assert.equal(egressBytes(0, locale), '0 B');
    assert.equal(egressAddress(historicalExit, locale), 'socks5h://old.proxy.test:1080');
    assert.equal(egressAddress({ scheme: 'http', host: '2001:db8::1', port: 8080 }, locale), 'http://[2001:db8::1]:8080');
    assert.equal(egressAddress({ scheme: 'https', host: '[2001:db8::1]', port: 443 }, locale), 'https://[2001:db8::1]:443');
    for (const host of ['user:credential-canary@proxy.test', 'proxy.test/?token=credential-canary', 'proxy.test/path', 'proxy.test\\credential-canary']) {
      assert.doesNotMatch(egressAddress({ scheme: 'socks5h', host, port: 1080 }, locale), /credential-canary/);
    }
    assert.equal(egressSelectionName(historicalExit, locale), 'Research then · Original exit');
    assert.doesNotMatch(egressSelectionName({ ...historicalExit, group_name: null, member_name: null }, locale), /group-a|member-a/);
  }
  assert.notEqual(egressSelectionKey(egressSummaries[0].selected), egressSelectionKey(egressSummaries[1].selected));
  assert.equal(egressGroupKey(egressSummaries[0].selected), egressGroupKey(egressSummaries[1].selected));
  const path = new URL(egressSummaryPath('tenant / &', 123, 456), 'https://fixture.invalid');
  assert.equal(path.pathname, '/internal/v1/requests/egress-summary');
  assert.equal(path.searchParams.get('tenant_external_id'), 'tenant / &');
  assert.equal(path.searchParams.get('start'), '123');
  assert.equal(path.searchParams.get('end'), '456');
  assert.equal(path.searchParams.has('group_id'), false);
});
