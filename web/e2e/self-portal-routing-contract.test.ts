import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { selfPortalRouteFromSearch, selfPortalRoutes, selfPortalSearchForRoute } from '../src/self/routes.js';

test('portal route adapter accepts only the six public view keys', () => {
  assert.deepEqual(selfPortalRoutes, ['overview', 'requests', 'sessions', 'usage', 'generations', 'generate']);
  assert.equal(selfPortalRouteFromSearch('?view=sessions'), 'sessions');
  assert.equal(selfPortalRouteFromSearch('?view=unknown'), 'overview');
  assert.equal(selfPortalSearchForRoute('overview'), '');
  assert.equal(selfPortalSearchForRoute('generate'), '?view=generate');
});

test('portal route adapter never carries credentials in its URL state', () => {
  for (const route of selfPortalRoutes) {
    const search = selfPortalSearchForRoute(route);
    assert.doesNotMatch(search, /token|credential|key/i);
  }
});

test('overview and generation creation remain separate page boundaries', async () => {
  const [overview, create, portal] = await Promise.all([
    readFile(new URL('../src/self/OverviewPage.tsx', import.meta.url), 'utf8'),
    readFile(new URL('../src/self/GeneratePage.tsx', import.meta.url), 'utf8'),
    readFile(new URL('../src/self/SelfPortal.tsx', import.meta.url), 'utf8'),
  ]);
  assert.doesNotMatch(overview, /generation-create|createGeneration|\/v1\/images\/generations/);
  assert.match(create, /generation-create/);
  assert.match(portal, /route\?: SelfPortalRoute/);
  assert.match(portal, /embedded\?: boolean/);
  assert.match(portal, /showNavigation\?: boolean/);
  assert.match(portal, /type=\{credentialVisible \? 'text' : 'password'\}/);
  assert.match(portal, /aria-pressed=\{credentialVisible\}/);
});

test('credential-bound portal work is abortable and remounts on identity generation', async () => {
  const [portal, requests, sessions, generate, generations, overview] = await Promise.all([
    readFile(new URL('../src/self/SelfPortal.tsx', import.meta.url), 'utf8'),
    readFile(new URL('../src/self/RequestsPage.tsx', import.meta.url), 'utf8'),
    readFile(new URL('../src/self/SessionsPage.tsx', import.meta.url), 'utf8'),
    readFile(new URL('../src/self/GeneratePage.tsx', import.meta.url), 'utf8'),
    readFile(new URL('../src/self/GenerationsPage.tsx', import.meta.url), 'utf8'),
    readFile(new URL('../src/self/OverviewPage.tsx', import.meta.url), 'utf8'),
  ]);
  assert.match(portal, /authSequence\.current \+= 1/);
  assert.match(portal, /detailController\.current\?\.abort\(\)/);
  assert.match(portal, /credentialView\.key_id.*credentialView\.credential_generation.*credentialScopeGeneration/s);
  assert.match(portal, /<Suspense key=\{credentialScopeKey\}/);
  assert.match(requests, /requestSequence/);
  assert.match(requests, /requestController\.current\?\.abort\(\)/);
  assert.match(sessions, /setSessions\(\[\]\)/);
  assert.match(generate, /setPrompt\(''\)/);
  assert.match(generations, /setJobs\(\[\]\)/);
  assert.match(generations, /startCompletionPolling\(refresh, 1_000\)/);
  assert.doesNotMatch(generations, /setInterval/);
  assert.match(overview, /api<KeyView>\('\/self\/v1\/key'/);
});

test('generation catalog selection cannot submit an arbitrary model', async () => {
  const create = await readFile(new URL('../src/self/GeneratePage.tsx', import.meta.url), 'utf8');
  assert.doesNotMatch(create, /<datalist/);
  assert.match(create, /<select value=\{model\}/);
  assert.match(create, /!catalogAvailable \|\| !selectedModel/);
  assert.match(create, /loadCatalog\(\)/);
});
