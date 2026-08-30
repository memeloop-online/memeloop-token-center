import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const table = await readFile(new URL('../src/components.tsx', import.meta.url), 'utf8');
const portal = await readFile(new URL('../src/self/SelfPortal.tsx', import.meta.url), 'utf8');
const operator = await readFile(new URL('../src/operator/Operator.tsx', import.meta.url), 'utf8');

test('request rows render only confirmed server-projected session semantics', () => {
  assert.match(table, /request\.session_context/);
  assert.match(table, /association === 'confirmed'/);
  assert.match(table, /session_name/);
  assert.match(table, /task_kind/);
  assert.match(table, /agent_id/);
  assert.match(table, /sessions\.unlinkedRequests/);
  assert.doesNotMatch(table, /prompt.*session|model.*sessionLabel/);
});

test('portal and operator can drill from a request into the exact session route', () => {
  assert.match(portal, /openSession\(sessionId: string\)/);
  assert.match(portal, /navigate\('sessions'\)/);
  assert.match(operator, /openSessionById\(sessionId: string\)/);
  assert.match(operator, /navigate\('sessions'\)/);
});
