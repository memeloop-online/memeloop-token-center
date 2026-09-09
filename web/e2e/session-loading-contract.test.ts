import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const source = await readFile(new URL('../src/operator/SessionMonitor.tsx', import.meta.url), 'utf8');
const list = source.slice(source.indexOf('async function loadSessions'), source.indexOf('async function selectSession'));
const select = source.slice(source.indexOf('async function selectSession'), source.indexOf('async function refreshSelected'));

test('session list reads cancel obsolete work and retain visible data on background failure', () => {
  assert.match(source, /const listRequests = useRef\(new LatestRequestGate\(\)\)/);
  assert.match(list, /const request = listRequests\.current\.begin\(\)/);
  assert.match(list, /AbortSignal\.any\(\[request\.signal, AbortSignal\.timeout\(15_000\)\]\)/);
  assert.match(list, /if \(!older && !background\) setSessions\(\[\]\)/);
  assert.equal((source.match(/listRequests\.current\.invalidate\(\)/g) ?? []).length, 2);
});

test('initial list work and SSE refreshes do not overlap and amplify slow database queries', () => {
  assert.match(source, /refreshInFlight\.current \|\| listInFlight\.current\) return/);
  assert.match(list, /listInFlight\.current = true/);
  assert.match(list, /listInFlight\.current = false/);
  assert.match(list, /if \(!background && refreshDirty\.current\) scheduleRefresh\(\)/);
  assert.match(source, /await loadSessions\(false, filtersRef\.current, true\);\s*if \(generation !== scopeGeneration\.current\) return/);
});

test('detail loading does not disable the usable list and has its own progress indicator', () => {
  assert.match(select, /setDetailLoading\(true\)/);
  assert.doesNotMatch(select, /setLoading\(/);
  assert.match(source, /!visibleDetail && detailLoading && <div className="empty" role="status"/);
  assert.match(source, /showDiagnosticIds loading=\{detailLoading\}/);
});
