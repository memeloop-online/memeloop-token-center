import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { defaultRequestRefreshInterval } from '../src/operator/traffic/requestRefresh.js';
import { sessionEventRefreshDelayMs } from '../src/operator/sessionRefresh.js';

const source = await readFile(new URL('../src/operator/SessionMonitor.tsx', import.meta.url), 'utf8');
const list = source.slice(source.indexOf('async function loadSessions'), source.indexOf('async function selectSession'));
const select = source.slice(source.indexOf('async function selectSession'), source.indexOf('async function refreshSelected'));
const scopeTransition = source.slice(
  source.indexOf('useEffect(() => {\n    scopeGeneration.current += 1;'),
  source.indexOf('  useEffect(() => {\n    if (!token.trim() || revision === 0)'),
);
const cleanup = scopeTransition.slice(scopeTransition.indexOf('return () =>'));
const cancel = source.slice(source.indexOf('function cancelListLoad'), source.indexOf('  const hasScope'));

test('session list reads cancel obsolete work and retain visible data on background failure', () => {
  assert.match(source, /const listRequests = useRef\(new LatestRequestGate\(\)\)/);
  assert.match(list, /const request = listRequests\.current\.begin\(\)/);
  assert.match(list, /apiRead<LogicalSessionListResponse>\(/);
  const failure = list.slice(list.indexOf('} catch (reason)'), list.indexOf('} finally'));
  assert.match(failure, /setError\(messageOf\(reason, t\('sessions\.loadFailed'\)\)\)/);
  assert.doesNotMatch(failure, /setSessions\(\[\]\)/, 'failed same-scope reads must preserve visible data');
  assert.match(source, /loadSessions\(false, filters, visibleSessions\.length > 0\)/, 'retry preserves an already loaded page as a background refresh');
  assert.match(source, /setSessions\(\[\]\); setListScope\(''\)/, 'scope and filter transitions still clear obsolete data');
  assert.match(scopeTransition, /listSequence\.current \+= 1;\s*listRequests\.current\.invalidate\(\)[\s\S]*void loadSessions\(false, filters\)/,
    'a scope/filter transition invalidates the old lane before its replacement read');
  assert.match(cleanup, /listSequence\.current \+= 1;\s*listRequests\.current\.invalidate\(\)/,
    'unmount invalidates an in-flight list read');
  assert.match(cancel, /if \(!listInFlight\.current\) return;\s*listSequence\.current \+= 1;\s*listRequests\.current\.invalidate\(\)[\s\S]*setLoading\(false\);\s*setRefreshing\(false\);/,
    'manual cancellation aborts the lane and restores usable controls');
});

test('initial list work and SSE refreshes do not overlap and amplify slow database queries', () => {
  assert.match(source, /refreshInFlight\.current \|\| listInFlight\.current\) return/);
  assert.match(list, /listInFlight\.current = true/);
  assert.match(list, /listInFlight\.current = false/);
  assert.match(list, /if \(!background && refreshDirty\.current\) scheduleRefresh\(\)/);
  assert.match(source, /const listLoaded = await loadSessions\(false, filtersRef\.current, true\);\s*if \(generation !== scopeGeneration\.current \|\| !autoRefreshRef\.current\) return;\s*if \(!listLoaded\) \{ restoreBatch\(\); return; \}/,
    'a failed drained list batch is restored before the next session cadence');
});

test('live refresh is explicitly opt-in, while session invalidation stays below the traffic render cadence', async () => {
  const operator = await readFile(new URL('../src/operator/hooks/useOperatorRequestStream.ts', import.meta.url), 'utf8');
  const operatorPage = await readFile(new URL('../src/operator/Operator.tsx', import.meta.url), 'utf8');
  assert.match(source, /\[autoRefresh, setAutoRefresh\] = useState\(false\)/);
  assert.match(source, /if \(!autoRefreshRef\.current \|\| refreshTimer/);
  assert.ok(sessionEventRefreshDelayMs <= 500, 'terminal session invalidation must settle within 500ms');
  assert.equal(defaultRequestRefreshInterval, 5_000, 'request-table rendering retains its default 5-second cadence');
  assert.match(source, /}, sessionEventRefreshDelayMs\)/);
  assert.match(operator, /if \(this\.listeners\.size === 0\) return;/, 'unmounted Sessions never accumulates raw events');
  assert.match(operator, /enqueueSessionEventIdentity\(this\.eventKeyIds\.current, event\);\s*this\.revision \+= 1;/);
  assert.match(operator, /batch\.current\?\.enqueue\(event\)/, 'traffic events remain on the bounded request batch');
  assert.match(operatorPage, /sessionEvents=\{stream\.sessionEvents\}/, 'Sessions owns the prompt event subscription');
  assert.match(source, /checked=\{autoRefresh\}/);
  assert.match(source, /<Checkbox checked=\{autoRefresh\}/);
  assert.match(source, /<Button appearance="secondary" disabled=\{loading \|\| refreshing \|\| detailLoading\}/);
  assert.match(source, /if \(detailInFlight\.current\) \{ detailRefreshDirty\.current = true; return; \}/, 'detail overlap remains dirty until it settles');
  assert.match(source, /for \(const event of batchDetailEvents\) dirtyDetailEvents\.current\.add\(event\);/, 'overlapping batch events are retained');
  assert.match(source, /sessions\.refreshNow/);
});

test('detail loading does not disable the usable list and has its own progress indicator', () => {
  assert.match(select, /setDetailLoading\(true\)/);
  assert.doesNotMatch(select, /setLoading\(/);
  assert.match(source, /!visibleDetail && detailLoading && <div className="empty" role="status"/);
  assert.match(source, /showDiagnosticIds loading=\{detailLoading\}/);
});
