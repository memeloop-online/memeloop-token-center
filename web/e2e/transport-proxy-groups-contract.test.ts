import assert from 'node:assert/strict';
import test from 'node:test';
import { ApiError } from '../src/api.js';
import { transportProxyError, transportProxyFailureKind, transportProxyRequest } from '../src/operator/transportProxyGroups.js';
import { transportProxyGroupCopy } from '../src/operator/transportProxyGroupCopy.js';
import { networkProxyAddressValid, networkProxyError, networkProxyLiveConcurrency } from '../src/operator/NetworkProxies.js';
import { networkProxyCopy } from '../src/operator/NetworkProxyCopy.js';
import { concurrencySnapshot, csilProxy } from './fixtures/transport-proxy-entities.js';

test('transport proxy failures distinguish editable validation, CAS, denied and uncertain outcomes without exposing response secrets', () => {
  const secret = 'socks5h://privateProxySecret@10.0.0.1:1080';
  for (const [status, code, expected] of [
    [400, 'invalid_request', 'validation'], [422, undefined, 'validation'],
    [409, 'proxy_group_version_conflict', 'conflict'], [409, 'proxy_group_binding_conflict', 'conflict'],
    [403, 'forbidden', 'denied'], [503, 'service_overloaded', 'unknown'], [200, undefined, 'unknown'],
  ] as const) {
    const reason = new ApiError(secret, status, code);
    assert.equal(transportProxyFailureKind(reason), expected);
    for (const locale of ['zh-CN', 'en']) assert.ok(!transportProxyError(reason, locale).includes(secret));
  }
  assert.equal(transportProxyFailureKind(new TypeError(secret)), 'unknown');
  assert.match(transportProxyError(new TypeError(secret)), /关闭页面后保存仍会完成/);
  const denied = transportProxyError(new ApiError(secret, 403, 'forbidden'));
  assert.match(denied, /请联系管理员检查访问权限/);
  assert.doesNotMatch(denied, /providers:write|当前租户的全局操作员|privateProxySecret/);
});

test('proxy group copy explains choices and uncertain saves without permission-success or delivery reports', () => {
  const chinese = transportProxyGroupCopy('zh-CN');
  const english = transportProxyGroupCopy('en');
  assert.equal(chinese.manage, '管理代理组');
  assert.equal(english.manage, 'Manage proxy groups');
  assert.equal(chinese.candidate(1), '网络出口 1');
  assert.equal(english.candidate(1), 'Network exit 1');
  assert.equal(english.groupSummary('Research', 1, 1), 'Research · 1 exit · 1 account');
  assert.equal(english.groupSummary('Research', 2, 0), 'Research · 2 exits · 0 accounts');
  assert.match(chinese.purpose, /请求继续使用当前出口/);
  assert.match(chinese.purpose, /独立可达性探测/);
  assert.match(english.purpose, /Requests keep the current exit/);
  assert.match(english.purpose, /separate reachability probe/);
  assert.match(chinese.closePending, /关闭页面后保存仍会完成/);
  assert.match(english.closePending, /Closing the page still lets the save finish/);
  for (const copy of [chinese, english]) {
    assert.doesNotMatch(JSON.stringify(copy), /已具备代理组管理权限|全局提供商管理权限|异步读回|自动重放|验收矩阵|本轮|待交付|worklog/i);
  }
  const failure = new TypeError('privateProxySecret');
  assert.equal(transportProxyError(failure, 'zh-CN', 'read'), '无法加载代理配置，请重试。');
  assert.equal(transportProxyError(failure, 'en', 'read'), 'Proxy settings could not be loaded. Try again.');
  assert.match(transportProxyError(failure, 'en'), /check whether your changes were saved before retrying/);
  assert.doesNotMatch(JSON.stringify(english), /[\u3400-\u9fff]/);
});

test('stopping client wait rejects even when fetch ignores abort; late response does not turn the result into success', async () => {
  const original = globalThis.fetch;
  let release!: (response: Response) => void;
  let requestSignal: AbortSignal | null | undefined;
  globalThis.fetch = async (_input, init) => {
    requestSignal = init?.signal;
    return new Promise<Response>(resolve => { release = resolve; });
  };
  try {
    const controller = new AbortController();
    const pending = transportProxyRequest('/fixture', 'fixture-only', { method: 'PUT', body: '{}', signal: controller.signal });
    controller.abort();
    await assert.rejects(pending, /result unknown/);
    assert.equal(requestSignal?.aborted, true);
    release(new Response('{}'));
    await assert.rejects(pending, /result unknown/);
  } finally { globalThis.fetch = original; }
});

test('network exit presentation uses exact CSIL addressing and only live unfinished request concurrency', () => {
  assert.equal(networkProxyLiveConcurrency(concurrencySnapshot, csilProxy.address), 1);
  assert.equal(networkProxyLiveConcurrency({ ...concurrencySnapshot, stale: true }, csilProxy.address), undefined);
  assert.equal(networkProxyLiveConcurrency({ ...concurrencySnapshot, samples_truncated: true }, csilProxy.address), undefined);
  assert.equal(networkProxyLiveConcurrency(concurrencySnapshot, { ...csilProxy.address, port: 1081 }), 0);
  assert.equal(networkProxyAddressValid('socks5h://100.64.0.16:1080'), true);
  assert.equal(networkProxyAddressValid('socks5h://user:privateProxySecret@100.64.0.16:1080'), true);
  for (const invalid of ['', 'http://100.64.0.16:1080', 'socks5h://100.64.0.16:1080/path', 'socks5h://100.64.0.16:1080/path?query', 'socks5h://100.64.0.16:1080 #fragment']) {
    assert.equal(networkProxyAddressValid(invalid), false, invalid);
  }
  assert.equal(networkProxyCopy('zh-CN').summary('CSIL'), 'CSIL');
  assert.equal(networkProxyCopy('en').summary('CSIL', 1, 2), 'CSIL · 1 group · 2 accounts');
  assert.doesNotMatch(JSON.stringify(networkProxyCopy('en')), /[\u3400-\u9fff]/);
  const secret = 'privateProxySecret';
  for (const locale of ['zh-CN', 'en']) {
    assert.equal(networkProxyError(new ApiError(secret, 409, 'network_proxy_version_conflict'), locale), networkProxyCopy(locale).errors.versionConflict);
    assert.doesNotMatch(networkProxyError(new ApiError(secret, 503), locale), new RegExp(secret));
  }
});
