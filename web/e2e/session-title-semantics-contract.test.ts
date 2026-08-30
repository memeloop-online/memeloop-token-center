import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const sessions = await readFile(new URL('../src/SessionViews.tsx', import.meta.url), 'utf8');

test('session titles use only explicitly reported names', () => {
  assert.match(sessions, /session\.session_name \|\| t\('sessions\.reportedNameMissing'\)/);
  assert.match(sessions, /declaredSessionName \|\| summary\?\.session_name \|\| t\('sessions\.reportedNameMissing'\)/);
  assert.doesNotMatch(sessions, /sessions\.sessionTitle/);
  assert.doesNotMatch(sessions, /declaredSessionName \|\| summary\?\.model/);
});

test('session identifiers remain inside diagnostic disclosures', () => {
  assert.match(sessions, /<details><summary>\{t\('sessions\.diagnostics'\)\}<\/summary><code>\{session\.session_id\}<\/code>/);
  assert.match(sessions, /showDiagnosticIds && <details className="session-diagnostics"/);
  assert.match(sessions, /reportedSessionId && <>/);
  assert.doesNotMatch(sessions, /reportedSession && <span>/);
  assert.doesNotMatch(sessions, /const title[^;]*session_id/);
});

test('semantic warnings and duration chart use localized product copy', () => {
  assert.match(sessions, /t\('sessions\.durationBars'\)/);
  assert.match(sessions, /t\('sessions\.parentEvidenceDegraded'\)/);
  assert.doesNotMatch(sessions, /locale\.startsWith\('zh'\) \?/);
});
