import assert from 'node:assert/strict';
import test from 'node:test';
import { parsePluginUiProjection } from '../src/plugins/uiProjection.js';

const policy = { pluginId: 'example', slotId: 'summary', allowedLinkOrigins: ['https://example.com'] };
function projection(components: unknown[]) { return { schema_version: 1, plugin_id: 'example', slot_id: 'summary', components }; }

test('closed projection accepts only bounded declarative components', () => {
  const value = projection([
    { kind: 'text', text: '<script>alert(1)</script>' },
    { kind: 'metric', label: 'Requests', value: '12' },
    { kind: 'status', label: 'Service', state: 'ok' },
    { kind: 'link', label: 'Documentation', href: 'https://example.com/docs' },
  ]);
  assert.deepEqual(parsePluginUiProjection(value, policy), value);
  assert.equal(parsePluginUiProjection({ ...value, schema_version: 2 }, policy), null);
  assert.equal(parsePluginUiProjection({ ...value, plugin_id: 'other' }, policy), null);
  assert.equal(parsePluginUiProjection({ ...value, slot_id: 'other' }, policy), null);
  assert.equal(parsePluginUiProjection({ ...value, html: '<b>extra</b>' }, policy), null);
  for (const component of [
    { kind: 'iframe', src: 'https://example.com' },
    { kind: 'text', text: 'safe', onClick: 'alert(1)' },
    { kind: 'text', text: 'x'.repeat(2049) },
    { kind: 'text', text: '\u202eevil' },
    { kind: 'metric', label: 'value', value: 12 },
    { kind: 'status', label: 'State', state: 'surprise' },
  ]) assert.equal(parsePluginUiProjection(projection([component]), policy), null);
  assert.equal(parsePluginUiProjection(projection(Array(33).fill({ kind: 'text', text: 'x' })), policy), null);
});

test('links require exact core-owned HTTPS origin without credentials or ambiguous syntax', () => {
  for (const href of ['javascript:alert(1)', 'data:text/html,test', '//example.com', '/logout', 'http://example.com',
    'https://example.com.evil.test', 'https://user:pass@example.com', 'https://example.com:444',
    'https://example.com\\@evil.test', 'https://example.com/\npath', 'https://example.com/\u202eevil']) {
    assert.equal(parsePluginUiProjection(projection([{ kind: 'link', label: 'Go', href }]), policy), null, href);
  }
  assert.equal(parsePluginUiProjection(projection([{ kind: 'link', label: 'Go', href: 'https://example.com/' }]), { ...policy, allowedLinkOrigins: [] }), null);
});
