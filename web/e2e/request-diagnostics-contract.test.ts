import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const components = await readFile(new URL('../src/components.tsx', import.meta.url), 'utf8');
const selfDrawers = await readFile(new URL('../src/self/SelfDrawers.tsx', import.meta.url), 'utf8');
const selfPortal = await readFile(new URL('../src/self/SelfPortal.tsx', import.meta.url), 'utf8');
const operatorRequests = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
const types = await readFile(new URL('../src/types.ts', import.meta.url), 'utf8');
const copyButton = await readFile(new URL('../src/CopyButton.tsx', import.meta.url), 'utf8');

test('request table exposes copyable durable IDs and its recorded token billing split', () => {
  assert.match(components, /RequestIdentifier/);
  assert.match(components, /request\.request_id/);
  assert.match(components, /<CopyButton value=\{requestId\} label=\{t\('common\.copy'\)\}/);
  assert.match(copyButton, /navigator\.clipboard\?\.writeText/);
  assert.match(components, /request\.tokenBreakdown/);
  assert.match(components, /requestTokenBreakdown/);
  assert.match(components, /cached_input_tokens !== undefined/);
  assert.match(components, /cache_write_tokens !== undefined/);
  assert.doesNotMatch(components, /cached_input_tokens \?\? 0/);
  assert.doesNotMatch(components, /cache_write_tokens \?\? 0/);
});

test('both Requests drawers reuse the durable request diagnostics surface and session drilldown', () => {
  assert.match(selfDrawers, /<RequestDiagnostics request=\{detail\}/);
  assert.match(selfDrawers, /onOpenSession/);
  assert.match(selfPortal, /setRequestDetail\(undefined\); openSession\(sessionId\)/);
  assert.match(operatorRequests, /<RequestDiagnostics request=\{detail\} onOpenSession=\{onOpenSession\}/);
  assert.match(components, /context\.association === 'confirmed'/);
  assert.match(components, /context\.session_id/);
  assert.match(components, /sessions\.unlinkedRequests/);
});

test('request diagnostics use only nullable server-recorded final routing fields', () => {
  const requestView = types.slice(types.indexOf('export interface RequestView'), types.indexOf('/** Exclusive descending keyset cursor'));
  for (const field of ['completed_at?: number | null', 'upstream_account_id?: string | null', 'route_id?: string | null', 'currency?: string | null']) {
    assert.match(requestView, new RegExp(field.replaceAll('?', '\\?').replaceAll('|', '\\|')));
  }
  assert.match(components, /request\.receivedAt/);
  assert.match(components, /request\.completedAt/);
  assert.match(components, /request\.upstreamId/);
  assert.match(components, /request\.routeId/);
  assert.match(components, /upstream_account_id \?\? '—'/);
  assert.match(components, /route_id \?\? '—'/);
  assert.match(components, /request\.currency === undefined \? fallbackCurrency : request\.currency/);
  assert.doesNotMatch(components, /routing_attempts|ttft|tokens_per_second/i);
});
