import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

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
  assert.match(source, /const listLoaded = await loadSessions\(false, filtersRef\.current, true\);\s*if \(!listLoaded \|\| generation !== scopeGeneration\.current \|\| !autoRefreshRef\.current\) return/);
});

test('live refresh is explicitly opt-in, rate-limited and does not interrupt detail reads', () => {
  assert.match(source, /\[autoRefresh, setAutoRefresh\] = useState\(false\)/);
  assert.match(source, /if \(!autoRefreshRef\.current \|\| refreshTimer/);
  assert.match(source, /}, 3_000\)/);
  assert.match(source, /checked=\{autoRefresh\}/);
  assert.match(source, /<Checkbox checked=\{autoRefresh\}/);
  assert.match(source, /<Button appearance="secondary" disabled=\{loading \|\| refreshing \|\| detailLoading\}/);
  assert.match(source, /if \(!session \|\| detailInFlight\.current\) return/);
  assert.match(source, /sessions\.refreshNow/);
});

test('detail loading does not disable the usable list and has its own progress indicator', () => {
  assert.match(select, /setDetailLoading\(true\)/);
  assert.doesNotMatch(select, /setLoading\(/);
  assert.match(source, /!visibleDetail && detailLoading && <div className="empty" role="status"/);
  assert.match(source, /showDiagnosticIds loading=\{detailLoading\}/);
});
