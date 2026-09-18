import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { latestDeclaredSessionName, sessionFallback, unnamedSessionName } from '../src/sessionTitles.js';

const sessions = await readFile(new URL('../src/SessionViews.tsx', import.meta.url), 'utf8');
const components = await readFile(new URL('../src/components.tsx', import.meta.url), 'utf8');

test('request table and diagnostics reuse declared names and the shared contextual fallback', () => {
  const diagnostics = components.slice(components.indexOf('export function RequestDiagnostics'), components.indexOf('export function RequestTable'));
  const table = components.slice(components.indexOf('export function RequestTable'));
  for (const surface of [diagnostics, table]) {
    assert.match(surface, /credentialDisplayName\(request\.credential_identity\.key_alias, t\)/);
    assert.match(surface, /session_name\?\.trim\(\) \|\| unnamedSessionName\(t, locale, request\.created_at, sessionCredential\)/);
    assert.doesNotMatch(surface, /sessions\.reportedNameMissing/);
    assert.match(surface, /context\.association === 'confirmed'/);
    assert.match(surface, /request-session-unlinked[^\n]*sessions\.unlinkedRequests/);
  }
});

test('whitespace declarations use retained credential and receipt time without inventing a title', () => {
  const translate = (key: string, variables?: Record<string, string | number>) => JSON.stringify({ key, variables });
  const time = Date.UTC(2026, 8, 16, 4, 0);
  const fallback = unnamedSessionName(translate, 'zh-CN', time, 'Operator credential');
  assert.equal('  Real session title  '.trim() || fallback, 'Real session title');
  assert.equal('   '.trim() || fallback, JSON.stringify({ key: 'sessions.unnamedSession', variables: { time: new Date(time).toLocaleString('zh-CN'), credential: 'Operator credential' } }));
  assert.equal(unnamedSessionName(translate, 'en', time), JSON.stringify({ key: 'sessions.unnamedSessionNoCredential', variables: { time: new Date(time).toLocaleString('en') } }));
  assert.equal(unnamedSessionName(translate, 'zh-CN', time, 'Operator credential', true), JSON.stringify({ key: 'sessions.compactContextSession', variables: { time: new Date(time).toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false }), credential: 'Operator credential' } }));
  assert.match(components, /const sessionDisplay = contextualSession \? unnamedSessionName\([^\n]+, true\) : sessionLabel/);
  assert.match(components, /DetailTooltip content=\{sessionLabel\}/);
});

test('session titles use reported names and shared activity plus credential context when absent', () => {
  assert.match(sessions, /session\.session_name\?\.trim\(\) \|\| unnamedSessionName/);
  assert.match(sessions, /declaredSessionName \|\| summary\?\.session_name\?\.trim\(\) \|\| unnamedSessionName/);
  assert.equal(sessions.match(/latestDeclaredSessionName\(detail\)/g)?.length, 2, 'inline detail and drawer share declared-name semantics');
  assert.doesNotMatch(sessions, /session_id\.slice\(-8\)/);
  assert.doesNotMatch(sessions, /sessions\.sessionTitle/);
  assert.doesNotMatch(sessions, /declaredSessionName \|\| summary\?\.model/);
});

test('retained context and execution names are selected by timestamp, not request-array order', () => {
  const requests = [
    { request_id: 'old', created_at: 10, session_context: { session_name: 'Older declaration' } },
    { request_id: 'new', created_at: 20, session_context: { session_name: '  Current declaration  ' } },
    { request_id: 'blank', created_at: 30, session_context: { session_name: '   ' } },
  ];
  assert.equal(latestDeclaredSessionName({ requests }), 'Current declaration');
  assert.equal(latestDeclaredSessionName({ requests: [...requests].reverse() }), 'Current declaration');
  assert.equal(latestDeclaredSessionName({ requests: [{ request_id: 'execution', created_at: 40, session_context: { session_name: ' ' }, execution: { session_name: 'Execution declaration' } }] }), 'Execution declaration');
  assert.equal(latestDeclaredSessionName({ requests: [] }), undefined);
  const sameTime = [{ request_id: 'a', created_at: 1, execution: { session_name: 'First' } }, { request_id: 'b', created_at: 1, execution: { session_name: 'Second' } }];
  assert.equal(latestDeclaredSessionName({ requests: sameTime }), latestDeclaredSessionName({ requests: [...sameTime].reverse() }));
});

test('fallback context is consistent with the list and never invents an epoch date for absent data', () => {
  const detail = { requests: [
    { request_id: 'older', created_at: 10, credential_identity: { key_alias: 'Older alias', key_id: 'key-a' } },
    { request_id: 'latest', created_at: 20, credential_identity: { key_alias: 'Retained alias', key_id: 'key-a' } },
  ] };
  assert.deepEqual(sessionFallback(detail), { time: 20, credential: 'Retained alias' });
  assert.deepEqual(sessionFallback({ requests: [...detail.requests].reverse() }), sessionFallback(detail));
  assert.deepEqual(sessionFallback(detail, { last_activity_at: 30, key_alias: ' ', key_id: 'summary-key' }), { time: 30, credential: 'summary-key' });
  assert.deepEqual(sessionFallback(detail, { last_activity_at: 30, key_alias: '__retired_credential__', key_id: 'internal-key' }), { time: 30, credential: undefined });
  assert.deepEqual(sessionFallback({ requests: [] }), { time: undefined, credential: undefined });
  const calls: Array<{ key: string; variables?: Record<string, string | number> }> = [];
  const translate = (key: string, variables?: Record<string, string | number>) => { calls.push({ key, variables }); return key; };
  assert.equal(unnamedSessionName(translate, 'en', undefined), 'sessions.logicalSession');
  assert.equal(unnamedSessionName(translate, 'en', undefined, 'Known credential'), 'sessions.contextSession');
  unnamedSessionName(translate, 'zh-CN', 20, 'Known credential');
  assert.deepEqual(calls.at(-1), { key: 'sessions.unnamedSession', variables: { time: new Date(20).toLocaleString('zh-CN'), credential: 'Known credential' } });
});

test('semantic warnings and duration chart use localized product copy', () => {
  assert.match(sessions, /t\('sessions\.durationBars'\)/);
  assert.match(sessions, /t\('sessions\.parentEvidenceDegraded'\)/);
  assert.doesNotMatch(sessions, /locale\.startsWith\('zh'\) \?/);
});
