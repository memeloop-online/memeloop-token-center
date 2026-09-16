import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const view = await readFile(new URL('../src/SessionViews.tsx', import.meta.url), 'utf8');
const monitor = await readFile(new URL('../src/operator/SessionMonitor.tsx', import.meta.url), 'utf8');
const credential = await readFile(new URL('../src/operator/SessionCredentialFilter.tsx', import.meta.url), 'utf8');

test('conversation content precedes optional operational timelines', () => {
  assert.ok(view.indexOf('<SessionReplayPanel detail=') < view.indexOf('<SessionActivity detail='));
  assert.match(view, /Disclosure title=\{t\('sessions.executionTimeline'\)\} defaultOpen=\{detail.unlinked\}/);
  assert.match(view, /className="session-event-model" tabIndex=\{0\}>\{request.model\}/);
  assert.match(view, /formatMetricDisplay/);
  assert.match(view, /formatCurrencyDisplay/);
  assert.match(view, /requestDisplayedCost/);
  assert.match(view, /formatDurationDisplay/);
  assert.doesNotMatch(view, /className="session-detail" role=\{onClose/);
});

test('request inspection retains the selected conversation', () => {
  assert.match(monitor, /onSelect=\{\(request\) => \{ void onSelectRequest\(request\); \}\}/);
  assert.match(monitor, /SessionCredentialFilter value=\{draft.keyId\}/);
  assert.match(credential, /optionValue/);
  assert.match(credential, /session.key_alias \|\| session.key_id/);
  assert.match(credential, /setCustomValidity\(''\);\s*\}, \[value, selected\?\.label, scope, id\]\)/);
});
